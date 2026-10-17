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
 * Process-scoped registry of the destructive provider operations whose
 * completion could NOT be verified, keyed per profile name AND per operation.
 *
 * WHY A NEW SESSION MUST NOT OPEN ON SUCH A PROFILE (the proof this object
 * exists for): the watchdog answers the CALLER — it never stops the provider
 * operation underneath. `CookieManager.removeAllCookies` offers no
 * cancellation, so after a timeout the deletion may still be in flight and
 * can complete *after* the site was reopened, silently removing the new
 * session's cookies (a fresh login, or the one-time jar import). Ignoring the
 * late callback result does not help: the data change happens regardless.
 * Refusing the open until the operation is known to have finished is the only
 * safe resolution, and it is recoverable.
 *
 * WHY ONE `name -> reason` RECORD IS NOT ENOUGH: the block has an OWNER. With
 * a single record per name, two races reopen a profile that is still unsafe:
 *  1. clear A's cookie wipe times out (block raised); clear B then runs and
 *     every step answers, so B's success "releases" the name — while A's
 *     native deletion is *still outstanding* and lands on the session that
 *     now opens the profile;
 *  2. A and B both time out; A's late callback "releases" the name even
 *     though B's deletion is still in flight.
 * So every timed-out destructive call is tracked by its own [StepGuard]
 * operation id, a late callback closes ONLY its own record, and a verified
 * wipe never erases somebody else's outstanding operation.
 *
 * THE BLOCK CONDITION (and the final-verification proof): a profile is
 * blocked while
 *  * at least one destructive operation is still outstanding ([pending]), OR
 *  * the newest fully verified wipe happened BEFORE the last outstanding
 *    operation settled ([State.verifiedAt] < [State.lastSettledAt]).
 * The second clause is what makes a late answer unable to declare the profile
 * safe on its own: closing the last outstanding operation moves the clock
 * forward, so any verification recorded earlier is stale by construction and
 * the site stays closed until a wipe is verified *after* the last destructive
 * operation settled. A counter alone would not be enough — clearing a counter
 * says nothing about whether the state was verified after the last settle.
 *
 * WHAT IS AND IS NOT TRACKED:
 *  * `removeAllCookies` timeout → tracked (ungated destructive, unverified).
 *  * geolocation timeout → NOT tracked: the mutation is ours and gated by
 *    [StepGuard]/[GeolocationWipe], so a late callback provably clears
 *    nothing; `getOrigins` is read-only.
 *  * `WebStorage.deleteAllData()` → synchronous: it returned or threw, so the
 *    outcome is verified by construction.
 *  * final `flush()` timeout → NOT tracked: `flush()` only persists the cookie
 *    store that already exists, so a late flush cannot delete or grant
 *    anything a new session created (worst case it durably writes that
 *    session's own cookies, which its own close flush would do anyway). It
 *    still fails the wipe, so Rust keeps the encrypted blobs for a retry.
 *
 * RETIREMENT (verified deletion + generation bump): when the provider accepts
 * a deletion — or reports the profile absent — the name is retired for good:
 * Rust bumps the site's generation and `resolve_queued_free_generation` never
 * hands that name out again, so no session can ever sit on it and its
 * outstanding operations become unreachable. [retire] therefore drops the
 * whole record set for that name, and the *new* generation starts unblocked.
 * A **deferred** deletion (Chromium refuses: profile in use) keeps every
 * record, because the profile still exists and could be reopened.
 *
 * WHY IN-MEMORY IS THE RIGHT SCOPE: the hazard is a late callback in THIS
 * process. Those callbacks belong to the app's browser process — a restart
 * destroys them, and the registry starts empty by construction. Persisting it
 * would lock the user out of a site across restarts for a hazard that no
 * longer exists. (The migration flow faces the same late-write problem and
 * answers it by RETIRING the name — see [SiteMigrationFlow]: a virgin import
 * profile is disposable, a profile the user clears in place is not.)
 *
 * Production never calls [reset] (test isolation only) and there is no blanket
 * "release this name" any more: the only ways a block disappears are the
 * [settle] of every outstanding operation followed by a [verified] wipe, or
 * [retire] of a name that is gone for good.
 */
object ProfileQuarantine {
  /**
   * Reported when nothing is outstanding but the last verified wipe predates
   * the last settled operation — i.e. the profile has not been re-verified
   * since it became safe to look at again.
   */
  const val STALE_VERIFICATION_REASON: String =
    "son doğrulama, bekleyen son çerez silme işlemi sonuçlanmadan önce yapıldı"

  /** Per-profile ownership ledger. All access is serialised by the monitor. */
  private class State {
    /** Outstanding unverified destructive operations: operation id → reason. */
    val pending = LinkedHashMap<Long, String>()

    /** Clock value when the last outstanding operation settled. */
    var lastSettledAt = 0L

    /** Clock value when a fully verified wipe last completed. */
    var verifiedAt = 0L
  }

  private val states = HashMap<String, State>()

  /**
   * Monotonic ordering source. It only ever has to answer "did the
   * verification happen after the last settle?", so a wall clock (which can
   * jump) is deliberately not used.
   */
  private val clock = AtomicLong(0)

  private fun stateOf(profileName: String): State =
    states.getOrPut(profileName) { State() }

  private fun pruneIfOpen(profileName: String, state: State) {
    if (state.pending.isEmpty() && state.verifiedAt >= state.lastSettledAt) {
      states.remove(profileName)
    }
  }

  /**
   * Record that the destructive operation [operationId] on [profileName]
   * timed out and is therefore still outstanding. Only the watchdog of THAT
   * operation calls this, so a timeout can never register somebody else's id.
   */
  @Synchronized
  fun quarantine(profileName: String, operationId: Long, reason: String) {
    stateOf(profileName).pending[operationId] = reason
  }

  /**
   * Close [operationId]'s own record: its provider callback finally answered,
   * so that operation is no longer in flight.
   *
   * Returns `false` for an id this profile does not have outstanding — an
   * unknown operation or a **duplicate callback**, which must not advance the
   * clock, must not close another operation's record and must not lift
   * anything. Returning `true` does NOT mean the profile is usable: see the
   * block condition above.
   */
  @Synchronized
  fun settle(profileName: String, operationId: Long): Boolean {
    val state = states[profileName] ?: return false
    if (state.pending.remove(operationId) == null) return false
    state.lastSettledAt = clock.incrementAndGet()
    return true
  }

  /**
   * Record that a wipe of [profileName] completed with **every** step
   * verified. This never removes an outstanding operation of an earlier
   * attempt: with one still pending the profile stays blocked, because that
   * deletion can yet land on the session a "successful" reopen would create.
   */
  @Synchronized
  fun verified(profileName: String) {
    val state = stateOf(profileName)
    state.verifiedAt = clock.incrementAndGet()
    pruneIfOpen(profileName, state)
  }

  /**
   * Drop every record for a name that is retired for good (a deletion the
   * provider accepted, or an absent profile): Rust bumps the generation, so
   * the name is never handed out again and its outstanding operations cannot
   * reach a new session. Returns how many were dropped. A *deferred* deletion
   * must not call this.
   */
  @Synchronized
  fun retire(profileName: String): Int {
    val dropped = states.remove(profileName)?.pending?.size ?: 0
    return dropped
  }

  /** Why [profileName] is blocked, or `null` when it may back a new session. */
  @Synchronized
  fun reasonFor(profileName: String): String? {
    val state = states[profileName] ?: return null
    if (state.pending.isNotEmpty()) {
      val first = state.pending.values.first()
      val extra = state.pending.size - 1
      return if (extra == 0) first else "$first (+$extra bekleyen işlem daha)"
    }
    if (state.verifiedAt < state.lastSettledAt) return STALE_VERIFICATION_REASON
    return null
  }

  @Synchronized
  fun isBlocked(profileName: String): Boolean = reasonFor(profileName) != null

  /** Outstanding operation ids for [profileName] (diagnostics and tests). */
  @Synchronized
  fun pendingOperations(profileName: String): List<Long> =
    states[profileName]?.pending?.keys?.toList() ?: emptyList()

  /** Every currently blocked name (diagnostics/tests). */
  @Synchronized
  fun names(): Set<String> = states.keys.filter { reasonFor(it) != null }.toSet()

  /**
   * Forget every record. TEST ISOLATION ONLY: no production path may call it,
   * because dropping other operations' records is exactly the unsafety the
   * per-operation ledger exists to prevent. Production uses [settle] (own
   * record), [verified] (own attempt) and [retire] (a name retired for good).
   */
  @Synchronized
  fun reset() {
    states.clear()
  }
}

/**
 * The open-time half of the quarantine: BOTH phases of the two-phase open
 * (`site_migrate` and `site_open`) consult this SAME check before they touch
 * the provider, so a profile with an outstanding destructive operation is
 * never bound to a WebView and never receives the one-time jar import. One
 * function, two call sites — a guarantee that existed on only one phase would
 * let the jar import write cookies straight into an in-flight deletion.
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
      port.scheduleTimeout(STEP_TIMEOUT_MS) {
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
        if (settled.compareAndSet(false, true)) next(ok) else onLateAnswer(guard)
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
