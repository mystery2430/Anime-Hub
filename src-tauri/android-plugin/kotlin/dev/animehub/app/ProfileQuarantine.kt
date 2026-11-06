// SPDX-License-Identifier: MIT
//
// Profile quarantine: the per-operation ledger of unverified destructive
// provider calls, the open-time gate that reads it, and the retirement rule
// that decides when a name is provably unreachable.
//
// Pure decision/sequencing code: this file must NOT import `android.*` or any
// Tauri class, so the CI unit-test source set compiles and runs it against the
// generated Android project (see SiteProfileOps.kt for the full rationale and
// the one-thread-per-flow rule). Split out of SiteProfileOps.kt so each
// responsibility — and what may change it — is visible at a glance.

package dev.animehub.app

import java.util.concurrent.atomic.AtomicLong

// ---------------------------------------------------------------------------
// Quarantine: per-operation ledger, open gate, retirement
// ---------------------------------------------------------------------------

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
   * Drop every record for a name that is retired for good. Returns how many
   * outstanding operations were dropped with it.
   *
   * CALL ONLY AFTER THE NAME IS PROVABLY UNREACHABLE — i.e. after Rust has
   * durably bumped and stored the site's generation (`bump_profile_generation`
   * inside `run_mobile_delete_protocol`), or after a queued/orphan deletion was
   * durably recorded. Acceptance by the provider is NOT that moment: the bump
   * can still fail, and then the same name is derived again by the next open.
   * See [ProfileRetirement] for the full ordering argument. A *deferred*
   * (`Pending`) deletion must not call this either — the profile still exists.
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

/**
 * The exact shape Rust's `android_profile_name` produces: `site-<32 hex>-g<N>`.
 * ONE definition, shared by the controller's validation and
 * [ProfileRetirement.refusal]. Two copies of this rule could drift, and a
 * drifted copy is how an unvalidated name reaches a destructive provider call.
 */
val SITE_PROFILE_NAME: Regex = Regex("""site-[0-9a-f]{32}-g\d{1,10}""")

/**
 * Whether a profile name's quarantine ledger may be retired right now.
 *
 * ORDER IS THE SAFETY PROPERTY, and it is Rust's to keep: the ledger may be
 * dropped ONLY once the site's generation bump is durably stored, because that
 * is what makes the name underivable forever. This controller used to retire
 * inside `deleteProfileData` — the moment the provider merely *accepted* the
 * deletion, before Rust had written anything. If the bump or its storage write
 * then failed, `remove_site` aborted with the registry intact, the next open
 * derived the SAME name, and the gate no longer knew about the
 * `removeAllCookies` deletion that could still be in flight on it: the exact
 * race [ProfileQuarantine] exists to prevent, re-opened by the cleanup path.
 * Rust now calls the separate `site_profile_retire` bridge operation after
 * `bump_profile_generation` succeeded (`run_mobile_delete_protocol`), so any
 * failure before that point leaves the name blocked and a retry safe.
 *
 * Pure, so CI executes the rule instead of trusting somebody's reading of it.
 */
object ProfileRetirement {
  /** Not a name this app could have derived: never addressed destructively. */
  const val INVALID_NAME: String = "geçersiz profil adı"

  /** A profile a live session is using keeps its ledger until it is closed. */
  const val ACTIVE_PROFILE: String = "aktif site profilinin karantinası emekliye ayrılamaz"

  /** `null` when the ledger may be retired, otherwise the refusal to report. */
  fun refusal(profileName: String, activeProfile: String?): String? =
    when {
      !SITE_PROFILE_NAME.matches(profileName) -> INVALID_NAME
      activeProfile == profileName -> ACTIVE_PROFILE
      else -> null
    }
}
