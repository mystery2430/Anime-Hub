/**
 * Android per-site WebView profile isolation — the parts that can be locked
 * without a device.
 *
 * The feature's contract, from `docs/android-storage-isolation.md`:
 *  - each registered site id maps to its own androidx.webkit profile,
 *    deterministically and never from the URL/display name;
 *  - the profile is assigned before any other WebView API / first navigation;
 *  - `WebViewFeature.MULTI_PROFILE` is checked at runtime and an unsupported
 *    provider REFUSES the open (no silent fallback to the default profile);
 *  - the legacy JSON localStorage/IndexedDB/cookie-swap machinery is gone;
 *  - migration is a one-time, profile-scoped cookie import before first
 *    navigation; snapshots are not reimported;
 *  - deletion never runs against the default profile, is queued when the
 *    provider refuses (in use), and a failed delete never reports success;
 *  - the launcher's default-profile data is untouched by site paths;
 *  - back goes: fullscreen → site history → launcher; never finishes the
 *    task; PiP re-binds between the site view and the launcher.
 *
 * Real multi-profile behaviour still needs a device — see the doc's
 * verification section. These are static/contract tests.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (p) => readFileSync(p, "utf8").replace(/\r\n/g, "\n");

/**
 * Strip full-line `//` comments so negative assertions test real code, not
 * documentation that (correctly) mentions the forbidden construct.
 */
const code = (src) =>
  src
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .split("\n")
    .filter((line) => !/^\s*\/\//.test(line))
    .join("\n");

const KOTLIN = "src-tauri/android-plugin/kotlin/dev/animehub/app";
/**
 * The pure Kotlin side, split by responsibility (SiteProfileOps.kt's header
 * lists the layout). `read` throws when a file is missing, so a dropped file
 * fails this suite instead of silently shrinking what is pinned.
 */
const PURE_FILES = [
  "SiteProfileOps.kt",
  "ProfileQuarantine.kt",
  "SiteCloseFlow.kt",
  "SiteShutdownFlow.kt",
  "SiteClearFlow.kt",
  "SiteMigrationFlow.kt",
  "SiteBackStatus.kt",
  // Connection pinning: the CONNECT proxy, the socket it dials and the gate
  // that decides whether a site may open at all. All three are java.net only.
  "DnsPinnedProxy.kt",
  "PinnedConnect.kt",
  "SiteProxyGate.kt",
];
const pureKotlin = PURE_FILES.map((f) => read(`${KOTLIN}/${f}`)).join("\n");
const pureFile = (f) => read(`${KOTLIN}/${f}`);
/**
 * The platform boundary: the single blocking-flush worker and the watchdog
 * arming. Not pure (it touches Handler/Looper/Activity) — which is exactly why
 * it lives outside the pure set.
 */
const flushWorkerKt = read(`${KOTLIN}/SiteFlushWorker.kt`);
const blocklistKt = read(`${KOTLIN}/SiteBlocklist.kt`);

const windowsRs = read("src-tauri/src/web/windows.rs");
const commandsRs = read("src-tauri/src/commands.rs");
const bridgeRs = read("src-tauri/src/android_bridge.rs");
const pluginLibRs = read("src-tauri/android-plugin/src/lib.rs");
const controllerKt = read(
  "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubSiteController.kt",
);
const pluginKt = read("src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt");
const pipKt = read("src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPipController.kt");
const preparePy = read("scripts/android_prepare.py");
const mainJs = read("src/main.js");

// ---------------------------------------------------------------------------
// Profile naming
// ---------------------------------------------------------------------------

test("profile names derive from site_id only, hashed, with a generation", () => {
  // The derivation hashes the registered id; no URL/display-name input exists.
  const start = windowsRs.indexOf("pub fn android_profile_name");
  assert.ok(start > 0, "android_profile_name must exist");
  const body = windowsRs.slice(start, windowsRs.indexOf("\n}", start));
  assert.match(body, /hasher\.update\(site_id\.as_bytes\(\)\)/);
  assert.match(body, /format!\("\{ANDROID_PROFILE_PREFIX\}\{\}+-g\{generation\}"|format!/);
  // Conservative charset shared with the Kotlin validator.
  assert.match(windowsRs, /pub const ANDROID_PROFILE_PREFIX: &str = "site-"/);
  // The generation comes from a persisted counter, not from memory.
  assert.match(windowsRs, /const PROFILE_GENERATION_PREFIX: &str = "profilegen-"/);
  // Kotlin enforces the exact shape, so a stray name can never reach the
  // default profile. The rule is defined ONCE — in SiteProfileOps.kt — and
  // shared by the controller's validation and the retirement decision.
  assert.match(
    pureKotlin,
    /val SITE_PROFILE_NAME: Regex = Regex\("""site-\[0-9a-f\]\{32\}-g\\d\{1,10\}"""\)/,
    "Kotlin must validate profile names against the Rust derivation",
  );
  // The rule is defined ONCE (SiteProfileOps.kt) and shared by the controller's
  // validation and the retirement decision, so the two cannot drift.
  assert.match(
    controllerKt,
    /fun profileNameIsValid\(name: String\): Boolean = SITE_PROFILE_NAME\.matches\(name\)/,
  );
  assert.match(
    pureKotlin,
    /val SITE_PROFILE_NAME: Regex = Regex\("""site-\[0-9a-f\]\{32\}-g\\d\{1,10\}"""\)/,
  );
  assert.doesNotMatch(
    code(controllerKt),
    /Regex\("""site-/,
    "the controller must not keep a second copy of the name rule",
  );
});

test("delete and clear refuse names outside the derivation", () => {
  // Both Kotlin entry points validate before touching the provider store.
  const deleteAt = controllerKt.indexOf("fun deleteProfileData");
  const clearAt = controllerKt.indexOf("fun clearProfileData");
  assert.ok(deleteAt > 0 && clearAt > 0);
  assert.match(controllerKt.slice(deleteAt, deleteAt + 400), /profileNameIsValid\(profileName\)/);
  assert.match(controllerKt.slice(clearAt, clearAt + 400), /profileNameIsValid\(profileName\)/);
  // Nothing in the plugin/controller ever names the default profile.
  assert.doesNotMatch(controllerKt, /"Default"/);
  assert.doesNotMatch(pluginKt, /deleteProfile\("Default"\)/);
});

// ---------------------------------------------------------------------------
// MULTI_PROFILE runtime gate — no silent fallback
// ---------------------------------------------------------------------------

test("both open phases gate on MULTI_PROFILE before anything else and never fall back", () => {
  // The check is the first thing EACH entry point does, ahead of
  // validation/UI work: phase 1 (migrate) and phase 2 (the view).
  for (const entry of ["fun migrate(", "fun open("]) {
    const at = controllerKt.indexOf(entry);
    assert.ok(at > 0, `${entry} missing`);
    const guardEnd = controllerKt.indexOf("activity.runOnUiThread", at);
    assert.ok(guardEnd > at, `${entry} UI dispatch missing`);
    const guard = controllerKt.slice(at, guardEnd);
    const supportCheck = guard.indexOf("if (!isSupported())");
    const profileCheck = guard.indexOf("if (!profileNameIsValid(profileName))");
    assert.ok(supportCheck > 0, `${entry} must check MULTI_PROFILE`);
    assert.ok(
      profileCheck > supportCheck,
      `${entry} must refuse an unsupported provider before profile or UI work`,
    );
    assert.match(guard, /onResult\(ERR_UNSUPPORTED_MULTI_PROFILE\)\s*return/);
  }
  // No path opens the site when the feature is missing: both phases reject,
  // and the delete refusal reports the same feature gap (clear uses the
  // shared SiteProfileErrors.UNSUPPORTED literal). Counted over code only —
  // the KDoc references are documentation, not branches.
  assert.equal(
    (code(controllerKt).match(/ERR_UNSUPPORTED_MULTI_PROFILE/g) || []).length,
    4,
    "declaration + migrate reject + open reject + delete refusal",
  );
  // Rust classifies exactly that sentinel into the user-facing message.
  assert.match(pluginLibRs, /pub const MULTI_PROFILE_SENTINEL: &str = "unsupported-multi-profile"/);
  assert.match(bridgeRs, /is_unsupported_multi_profile\(&error\)/);
  assert.match(
    bridgeRs,
    /AppError::Unsupported\(\s*MULTI_PROFILE_UNSUPPORTED_MSG\.to_string\(\),?\s*\)/,
  );
  assert.match(bridgeRs, /pub const MULTI_PROFILE_UNSUPPORTED_MSG: &str = "/);
  // And open_site surfaces it to the UI unchanged (no logging-and-continue).
  assert.match(commandsRs, /windows::open_site_window\(/);
});

test("the unsupported-provider message tells the user what to do", () => {
  const msg = bridgeRs.match(/MULTI_PROFILE_UNSUPPORTED_MSG: &str = "([^"]+)"/);
  assert.ok(msg, "message constant missing");
  assert.match(msg[1], /güncelleyin/i, "must tell the user to update the WebView");
  assert.match(msg[1], /MULTI_PROFILE/);
  assert.match(msg[1], /desteklemiyor/i);
});

// ---------------------------------------------------------------------------
// setProfile ordering
// ---------------------------------------------------------------------------

test("setProfile runs before any other WebView API, attach and navigation", () => {
  const buildAt = controllerKt.indexOf("private fun buildWebView(");
  assert.ok(buildAt > 0);
  const construct = controllerKt.indexOf("webView = WebView(activity)", buildAt);
  const setProfile = controllerKt.indexOf("WebViewCompat.setProfile(webView, profileName)", buildAt);
  const settings = controllerKt.indexOf("applySiteSettings(WebSettingsSink(webView.settings))", buildAt);
  const docStart = controllerKt.indexOf("WebViewCompat.addDocumentStartJavaScript", buildAt);
  const addView = controllerKt.indexOf("content.addView(", buildAt);
  const loadUrl = controllerKt.indexOf("webView.loadUrl(url)", buildAt);
  assert.ok(construct > buildAt, "constructor missing");
  assert.ok(
    construct < setProfile,
    "the constructor must come first, then setProfile",
  );
  for (const [name, at] of [
    ["settings", settings],
    ["document-start script", docStart],
    ["addView", addView],
    ["loadUrl", loadUrl],
  ]) {
    assert.ok(setProfile < at, `${name} must come after setProfile`);
  }
  // Nothing else can have touched the view before: no other API call sits
  // between the constructor and setProfile.
  const between = controllerKt.slice(construct, setProfile);
  assert.doesNotMatch(between, /webView\./, "no WebView API call may sit before setProfile");
});

test("the launcher WebView is never assigned a site profile", () => {
  // The wry WebView keeps the default profile: setProfile is only ever
  // called on the freshly constructed site view.
  const setProfiles = controllerKt.match(/WebViewCompat\.setProfile\([^)]*\)/g) || [];
  assert.equal(setProfiles.length, 1);
  assert.equal(setProfiles[0], "WebViewCompat.setProfile(webView, profileName)");
  // bindLauncher only stores a reference.
  const bind = controllerKt.slice(
    controllerKt.indexOf("fun bindLauncher"),
    controllerKt.indexOf("fun activeProfile"),
  );
  assert.doesNotMatch(bind, /setProfile/);
});

// ---------------------------------------------------------------------------
// Cookie handling: profile CookieManager only, one-time migration
// ---------------------------------------------------------------------------

test("site cookies never touch the static CookieManager", () => {
  // The static instance is the DEFAULT (launcher) profile. Site cookie work
  // must go through Profile.getCookieManager() / WebViewCompat.getProfile().
  assert.doesNotMatch(code(controllerKt), /CookieManager\.getInstance\(\)/);
  assert.match(controllerKt, /profile\.cookieManager/);
  // The migration port binds its manager from the profile it just created.
  assert.match(controllerKt, /manager = store\.getOrCreateProfile\(profileName\)\.cookieManager/);
  assert.match(controllerKt, /profile\.cookieManager\.flush\(\)/);
});

test("migration is one-time, profile-scoped, and ordered before first navigation", () => {
  // Rust side: `run_mobile_open_protocol` owns migrate → durable flag → open.
  const protoAt = windowsRs.indexOf("fn run_mobile_open_protocol_inner");
  assert.ok(protoAt > 0, "the host-tested two-phase protocol body is missing");
  const body = windowsRs.slice(protoAt, windowsRs.indexOf("\n}\n", protoAt));
  assert.match(body, /MIGRATION_FLAG_PREFIX/, "migration must be flag-gated");
  assert.match(body, /mark_migration_done/, "the protocol writes the flag");
  assert.match(body, /ops\.migrate\(/, "phase 1 migrates without a view");
  assert.match(body, /ops\.open\(/, "phase 2 builds the view");
  // THE ordering contract (audit defect D): the flag becomes durable AFTER a
  // strict migration success and BEFORE any view exists.
  assert.ok(
    body.indexOf("ops.migrate(") < body.indexOf("mark_migration_done"),
    "the flag may only follow a completed migration",
  );
  assert.ok(
    body.indexOf("mark_migration_done") < body.indexOf("ops.open("),
    "the view may only be built after the flag is durable",
  );
  // A failed flag write ABORTS the open — never the old log-and-continue,
  // which left a shown profile with a missing flag and got it retired.
  assert.match(body, /site migration state could not be stored; site was not opened/,
    "a failed flag write must abort before opening the view");
  assert.doesNotMatch(
    body,
    /log::warn!\("profil göç bayrağı yazılamadı/,
    "a failed flag write must not be a mere warning any more",
  );
  // `open_on_mobile` delegates and claims the session only afterwards.
  // The signature now also carries the per-opening identity, so match the
  // parameter list instead of one exact single-line spelling.
  const openSignature = windowsRs.match(
    /fn open_on_mobile\([\s\S]*?\) -> AppResult<String> \{/,
  );
  const openAt = openSignature?.index ?? -1;
  assert.ok(openAt > 0, "android open_on_mobile missing");
  const openBody = windowsRs.slice(openAt, windowsRs.indexOf("\n}\n", openAt));
  assert.match(openBody, /run_mobile_open_protocol\(/);
  assert.ok(
    openBody.indexOf("run_mobile_open_protocol(") < openBody.indexOf("Some(CurrentSite {"),
    "current_site is claimed only after the protocol succeeded",
  );
  // Kotlin: virgin profile only — an existing profile's live cookies are
  // authoritative and the migration REFUSES (retryable sentinel) instead of
  // being overwritten by the stale snapshot (A.3).
  assert.match(controllerKt, /MigrationOutcome\.ProfileAlreadyExists/);
  assert.match(controllerKt, /onResult\("\$ERR_MIGRATION_RETRY: profile already exists"\)/);
  // Flush to disk completes inside the migration flow, before buildWebView.
  assert.match(controllerKt, /m\.flush\(\)/);
  const flushAt = controllerKt.indexOf("m.flush()");
  const buildAt = controllerKt.indexOf("private fun buildWebView(");
  assert.ok(flushAt > 0 && flushAt < buildAt,
    "flush must run during migration, before buildWebView");
  // Any incomplete attempt reports the Rust-visible retry sentinel.
  assert.match(controllerKt, /const val ERR_MIGRATION_RETRY = "migration-partial"/);
  assert.match(windowsRs, /animehub_android::MIGRATION_RETRY_SENTINEL/);
  // localStorage/IndexedDB snapshots are NOT reimported anywhere.
  assert.doesNotMatch(windowsRs, /import_site_storage|localstorage_import|indexeddb_import/);
  assert.doesNotMatch(pluginKt, /fun localstorage_|fun indexeddb_/);
  // The blobs themselves survive until the user clears/removes the site.
  assert.match(read("src-tauri/src/secure/store.rs"), /"cookies", "localstorage", "indexeddb", "profilemig"/);
});

// ---------------------------------------------------------------------------
// Removal of the legacy JSON transfer machinery
// ---------------------------------------------------------------------------

test("the legacy JSON storage/cookie-swap machinery is gone", () => {
  for (const gone of [
    "export_site_storage",
    "import_site_storage",
    "rollback_android_cookie_swap",
    "provider_cookie_url",
    "STORAGE_SYNC_EVERY_N_TICKS",
    "webview_cookies_get",
    "webview_cookies_replace",
    "webview_cookies_clear",
    "localstorage_export",
    "localstorage_import",
    "indexeddb_export",
    "indexeddb_import",
    "parse_android_cookie_header",
  ]) {
    assert.ok(
      !windowsRs.includes(gone),
      `windows.rs still references the removed machinery: ${gone}`,
    );
    assert.ok(
      !commandsRs.includes(gone),
      `commands.rs still references the removed machinery: ${gone}`,
    );
  }
  // Kotlin side: commands and helpers gone.
  assert.doesNotMatch(pluginKt, /fun localstorage_|fun indexeddb_|fun webview_cookies_|EXPORT_IDB_JS|flushCookieStoreAsync/);
  // The periodic sync no longer exports anything on Android: it flushes the
  // active profile's cookies instead.
  const syncStart = windowsRs.indexOf('fn spawn_session_sync(app: AppHandle, _label: String, site_id: String, _host: String)');
  assert.ok(syncStart > 0, "android flush-only sync missing");
  const syncBody = windowsRs.slice(syncStart, windowsRs.indexOf("\n}\n", syncStart));
  assert.match(syncBody, /site_flush\(\)/);
  assert.doesNotMatch(syncBody, /export/);
});

test("no path clears the default profile's cookie jar for a site operation", () => {
  // `CookieManager.getInstance()` remains only in MainActivity's destroy-time
  // flush of the launcher's own (default) profile.
  const mainActivity = preparePy;
  assert.match(mainActivity, /android\.webkit\.CookieManager\.getInstance\(\)\.flush\(\)/);
  // Site paths never reference it — checked above for the controller and here
  // for the whole plugin file plus commands/windows.
  assert.doesNotMatch(code(pluginKt), /CookieManager\.getInstance/);
  assert.doesNotMatch(commandsRs, /webview_cookies_clear/);
});

// ---------------------------------------------------------------------------
// Deletion semantics
// ---------------------------------------------------------------------------

test("remove_site deletes the profile, queues refusals, bumps the generation", () => {
  const nonDesktop = commandsRs.indexOf("#[cfg(not(desktop))]");
  assert.ok(nonDesktop > 0);
  const body = commandsRs.slice(nonDesktop, commandsRs.indexOf("pub fn set_site_hidden", nonDesktop));
  // Builtin → hide and return BEFORE any profile deletion.
  const builtinAt = body.indexOf("if was_builtin");
  const deleteAt = body.indexOf("run_mobile_delete_protocol");
  assert.ok(builtinAt > 0 && deleteAt > builtinAt, "profile delete must come after the builtin check");
  // The builtin early-return happens before the delete call (and reports
  // the explicit `hidden` outcome, never a bare success).
  assert.ok(
    body.slice(builtinAt, deleteAt).includes("return Ok(RemoveSiteOutcome"),
    "builtin must return its outcome before deletion",
  );
  // The outcome flows from the pure mapping: Pending → deferred.
  assert.ok(
    body.includes("profile_outcome = profile_removal_for(action)"),
    "remove_site must report through profile_removal_for",
  );
  assert.match(
    commandsRs,
    /SiteProfileAction::Pending => ProfileRemoval::Deferred/,
    "Pending must map to deferred, never to deleted",
  );
  // The delete → queue → bump → retire order lives in ONE host-tested function
  // (windows.rs), with the platform calls injected — so `cargo test` proves it
  // on every OS instead of leaving it to the aarch64 compile.
  assert.match(body, /windows::run_mobile_delete_protocol\(/);
  assert.match(body, /crate::android_bridge::site_profile_delete\(profile\)/);
  assert.match(body, /crate::android_bridge::site_profile_retire\(profile\)/);
  assert.doesNotMatch(
    body,
    /w::bump_profile_generation/,
    "the command must not re-implement the protocol inline",
  );
  // Active site is closed (destroyed) before the delete.
  assert.ok(body.indexOf("close_site_window_for_clear_locked") < deleteAt);
  // Registry mutation follows the delete (the first persist in the block is
  // the builtin early-return, which must stay before it): a failed delete
  // keeps the tile.
  const persistAfterDelete = body.indexOf("persist_registry(&state)?;", deleteAt);
  assert.ok(persistAfterDelete > deleteAt, "the user-site registry write must follow the delete");
});

test("the quarantine ledger is retired only AFTER the generation is durable", () => {
  const ops = pureKotlin;
  // windows.rs: tombstone (WITH ITS OWNER) → delete → secure the generation →
  // lift → retire, all on the derived name. The tombstone is first because a
  // process kill inside the (slow) provider call must not leave a name that is
  // still derivable, still on disk and no longer remembered — see AH-04 / R-07.
  const at = windowsRs.indexOf("fn run_mobile_delete_protocol_inner");
  assert.ok(at > 0, "the delete protocol is missing");
  const body = windowsRs.slice(at, windowsRs.indexOf("\n}\n", at));
  const queue = body.indexOf("queue_site_profile_delete(state, site_id, &profile)");
  const del = body.indexOf("delete_profile(&profile)");
  const bump = body.indexOf("secure_generation_past_name(state, site_id, &profile)");
  const forget = body.indexOf("forget_pending_profile_delete(state, &profile)");
  const retire = body.indexOf("retire_profile(&profile)");
  assert.ok(queue > 0, "the durable intent is written before anything else");
  assert.ok(del > queue, "the provider is only called once the tombstone is durable");
  assert.ok(bump > del, "the durable generation follows the deletion");
  assert.ok(forget > bump, "the tombstone is lifted only after a confirmed deletion");
  assert.ok(retire > forget, "THE ORDER: retirement last");
  // The tombstone carries the site id: a profile name is a one-way digest, so
  // an ownerless record can never secure a generation at the next start.
  assert.doesNotMatch(
    body,
    /queue_pending_profile_delete\(/,
    "the delete protocol must record the owner, not queue a bare name",
  );
  assert.doesNotMatch(
    body,
    /bump_profile_generation\(/,
    "a blind bump is not idempotent; the generation is secured past THIS name",
  );
  // A Pending profile still exists on disk → it returns before the lift, so
  // tombstone and ledger both survive until the startup drain lands it.
  const pendingAt = body.indexOf("SiteProfileAction::Pending");
  assert.ok(pendingAt > del && pendingAt < forget, "a deferred deletion keeps its tombstone");
  assert.match(body, /return Ok\(action\);/);
  // A failed provider call propagates WITHOUT lifting the tombstone, so the
  // deletion is retried at the next start instead of being forgotten.
  assert.match(body, /Err\(error\) => \{[\s\S]{0,500}?return Err\(error\);/);
  // Host tests that execute the order itself.
  for (const name of [
    "mobile_delete_protocol_retires_only_after_the_generation_is_durable",
    "mobile_delete_protocol_keeps_the_ledger_when_the_generation_write_fails",
    "mobile_delete_protocol_queues_a_deferred_deletion_and_keeps_its_ledger",
    "mobile_delete_protocol_is_safe_to_retry_after_a_failed_bump",
    "mobile_delete_protocol_retires_each_generation_exactly_once_on_repeat",
    "mobile_delete_protocol_tombstones_the_name_before_the_provider_is_called",
    "a_failed_provider_delete_keeps_the_tombstone_so_the_name_cannot_come_back",
    // The startup half of the same protocol (P1: the kill between the
    // tombstone and the generation write).
    "drain_after_a_kill_before_the_provider_ran_finishes_the_deletion",
    "drain_after_a_kill_between_the_delete_and_the_generation_write_never_reopens_the_name",
    "drain_after_a_kill_between_the_generation_write_and_the_lift_does_not_bump_again",
    "drain_lifts_the_tombstone_when_retirement_fails_afterwards",
    "every_kill_point_leaves_the_next_open_with_a_different_name",
    "a_failed_generation_write_keeps_the_tombstone_queued",
    "repeated_drains_do_not_consume_another_generation",
    "a_legacy_names_only_queue_is_read_and_matched_against_the_registry",
    "an_unmatched_tombstone_is_kept_and_reported_not_silently_dropped",
    "an_orphan_tombstone_becomes_a_permanent_block_instead_of_lifting",
    "an_orphan_tombstone_whose_site_came_back_secures_the_generation",
    "orphan_sweep_tombstones_before_the_provider_and_blocks_before_retiring",
    "orphan_sweep_never_calls_the_provider_when_the_tombstone_write_fails",
    "orphan_sweep_keeps_the_tombstone_when_the_bridge_never_answers",
    "orphan_sweep_keeps_a_deferred_orphan_owed_for_the_startup_drain",
    "orphan_sweep_blocks_a_confirmed_name_for_deleted_and_absent_alike",
    "orphan_sweep_keeps_names_owed_when_the_provider_has_no_profile_api",
    "a_blocked_orphan_is_never_asked_again_and_never_dropped",
    "a_blocked_orphan_whose_site_id_comes_back_secures_a_new_generation",
    "a_failed_block_write_keeps_the_orphan_tombstone_owed",
    "repeated_orphan_sweeps_and_drains_consume_no_generation",
    "a_queue_object_written_before_the_block_state_reads_as_owed",
    "a_conflicting_owner_keeps_the_tombstone",
    "drain_survives_an_unsupported_provider_and_an_unready_bridge",
    "generation_past_name_is_monotone_idempotent_and_overflow_safe",
    "match_queued_name_to_site_needs_exactly_one_owner",
    "queue_records_the_owner_and_upgrades_a_legacy_entry",
  ]) {
    assert.match(windowsRs, new RegExp(`fn ${name}\\(\\)`), `missing host test ${name}`);
  }
  // Kotlin: acceptance by the provider no longer retires anything.
  const deleteFn = code(
    controllerKt.slice(
      controllerKt.indexOf("fun deleteProfileData("),
      controllerKt.indexOf("fun retireProfileQuarantine("),
    ),
  );
  assert.doesNotMatch(
    deleteFn,
    /ProfileQuarantine\.retire\(/,
    "deleteProfileData must not retire on mere acceptance",
  );
  assert.match(
    controllerKt,
    /fun retireProfileQuarantine\(\s*profileName: String,\s*operationId: String,\s*onResult: \(dropped: Int, error: String\?\) -> Unit/,
  );
  assert.match(controllerKt, /ProfileRetirement\.refusal\(profileName, active\.activeProfile\)/);
  assert.match(ops, /object ProfileRetirement/);
  assert.match(ops, /fun refusal\(profileName: String, activeProfile: String\?\): String\?/);
  assert.match(ops, /!SITE_PROFILE_NAME\.matches\(profileName\) -> INVALID_NAME/);
  assert.match(ops, /activeProfile == profileName -> ACTIVE_PROFILE/);
  // Bridge + plugin surface for the new, separately-sequenced operation.
  assert.match(bridgeRs, /pub fn site_profile_retire\(profile: &str\) -> AppResult<u32>/);
  assert.match(
    pluginLibRs,
    /pub const CMD_SITE_PROFILE_RETIRE: &str = "site_profile_retire"/,
  );
  assert.match(pluginLibRs, /pub struct SiteProfileRetireResult/);
  assert.match(pluginKt, /fun site_profile_retire\(invoke: Invoke\)/);
  assert.match(pluginKt, /AnimeHubSiteController\.retireProfileQuarantine\(profileName, operationId\)/);
  // The other two deletion paths keep the same rule: durable step first. The
  // orphan sweep no longer retires inline — it hands both provider calls to the
  // host-tested `run_orphan_profile_sweep`, which retires ONLY after the
  // permanent reuse block is durable (pinned by the ordering assertions in the
  // queue test below and driven by
  // `orphan_sweep_tombstones_before_the_provider_and_blocks_before_retiring`).
  const sweep = commandsRs.slice(commandsRs.indexOf('orphan sweep for "clear all"'));
  assert.match(
    sweep,
    /windows::run_orphan_profile_sweep\(/,
    "the orphan sweep still retires what it deleted, through the ordered helper",
  );
  assert.match(sweep, /report\.retired/, "the sweep reports what it retired");
  const libRs = read("src-tauri/src/lib.rs");
  const drain = libRs.slice(libRs.indexOf("fn drain_pending_profile_deletes"));
  assert.ok(
    drain.indexOf("forget_pending_profile_delete") < drain.indexOf("site_profile_retire"),
    "the startup drain forgets the queued name durably before retiring it",
  );
});

test("clear_site_data clears profiles in place and sweeps orphans, never the launcher", () => {
  const clearAt = commandsRs.indexOf("pub async fn clear_site_data");
  assert.ok(clearAt > 0);
  const body = commandsRs.slice(clearAt, commandsRs.indexOf("\n}\n", clearAt));
  assert.match(
    body,
    /windows::run_mobile_clear_protocol\(&state, id, \|profile\| \{/,
    "the per-site wipe runs through the host-tested clear protocol",
  );
  assert.match(body, /site_clear_data\(profile\)/, "per-site profile clear");
  // The session drop lives INSIDE that protocol (wipe first, blobs second) on
  // Android, and stays a direct call everywhere else — so a successful wipe can
  // never skip `delete_site_session`.
  assert.match(
    body,
    /#\[cfg\(not\(target_os = "android"\)\)\]\s+state\.provider\.delete_site_session\(id\)\?;/,
  );
  const clearProtoAt = windowsRs.indexOf("fn run_mobile_clear_protocol_inner");
  assert.ok(clearProtoAt > 0, "the host-tested clear body is missing");
  const clearProtocol = windowsRs.slice(clearProtoAt, windowsRs.indexOf("\n}\n", clearProtoAt));
  const profileWipeAt = clearProtocol.indexOf("clear_profile(&profile)");
  const sessionDropAt = clearProtocol.indexOf("state.provider.delete_site_session(site_id)");
  assert.ok(profileWipeAt > 0 && sessionDropAt > profileWipeAt,
    "wipe the profile first, then drop the encrypted blobs");
  assert.match(clearProtocol, /if let Err\(error\) = clear_profile\(&profile\)[\s\S]*?return Err\(error\)/,
    "a failed profile wipe keeps the encrypted session for retry");
  // Close-without-flush precedes the wipes for the active site.
  assert.ok(body.indexOf("close_site_window_for_clear_locked") < body.indexOf("site_clear_data"));
  // Orphan sweep (is_all) addresses only our namespace and never owned names.
  const sweepAt = body.indexOf("site_profile_names");
  assert.ok(sweepAt > 0, "android orphan sweep missing");
  assert.match(body.slice(sweepAt), /name\.starts_with\(windows::ANDROID_PROFILE_PREFIX\)/);
  assert.match(body.slice(sweepAt), /owned\.contains\(name\)/);
  // No shared/default-jar clearing call anywhere in the command.
  assert.doesNotMatch(body, /webview_cookies_clear/);
});

test("deferred deletions are persisted, drained at startup, and deduped", () => {
  assert.match(windowsRs, /pub const PENDING_PROFILE_DELETES_KEY: &str = "android-pending-profile-deletes"/);
  assert.match(windowsRs, /pub fn queue_pending_profile_delete/);
  assert.match(windowsRs, /pub fn queue_site_profile_delete/);
  assert.match(windowsRs, /pub fn queue_orphan_profile_delete/);
  assert.match(windowsRs, /pub fn block_pending_profile_delete/);
  assert.match(windowsRs, /pub fn forget_pending_profile_delete/);
  // lib.rs drains before the frontend can open a site.
  const libRs = read("src-tauri/src/lib.rs");
  const manageAt = libRs.indexOf("app.manage(AppState {");
  const drainAt = libRs.indexOf("drain_pending_profile_deletes");
  const routeAt = libRs.indexOf("// Route the OAuth redirect");
  assert.ok(manageAt > 0 && drainAt > manageAt, "drain must start after the state is managed");
  assert.ok(drainAt < routeAt, "drain spawns during setup, not later");
  assert.match(libRs, /fn drain_pending_profile_deletes/);
  // A pending result is never reported as deleted — the rule now lives in the
  // host-tested drain, so pin it there (where `cargo test` executes it).
  assert.match(
    windowsRs,
    /if matches!\(action, SiteProfileAction::Pending\) \{\s*\/\/ Still in use/,
  );
});

test("the startup drain proves a name is underivable before lifting its tombstone", () => {
  // P1 / R-07: a kill between the tombstone write and the generation write used
  // to leave the stored generation behind — the drain deleted the profile,
  // lifted the tombstone, and the next open derived the SAME name.
  const at = windowsRs.indexOf("fn run_pending_delete_drain_inner");
  assert.ok(at > 0, "the drain is missing from windows.rs");
  const body = windowsRs.slice(at, windowsRs.indexOf("\n}\n", at));
  const del = body.indexOf("delete_profile(&entry.name)");
  const secure = body.indexOf("secure_tombstone_generation(");
  const forget = body.indexOf("forget_pending_profile_delete(state, &entry.name)");
  const retire = body.indexOf("retire_profile(&entry.name)");
  assert.ok(del > 0, "the provider delete comes first");
  assert.ok(secure > del, "the generation is secured after a confirmed deletion");
  assert.ok(forget > secure, "THE ORDER: the tombstone is lifted only once the name is unreachable");
  assert.ok(retire > forget, "the ledger is retired last");
  // A deferred deletion returns before the lift.
  const pending = body.indexOf("SiteProfileAction::Pending");
  assert.ok(pending > del && pending < forget, "a deferred deletion keeps its tombstone");
  // An owner that cannot be established is never dropped and never assumed safe.
  assert.match(body, /PendingOwner::Unknown => match &matched \{/);
  assert.match(body, /kept_unmatched\.push\(entry\.name\.clone\(\)\)/);
  assert.match(body, /report\.kept_unmatched/);
  // Overflow keeps the tombstone instead of making the profile reachable again.
  assert.match(windowsRs, /checked_add\(1\)/);
  assert.match(
    windowsRs,
    /profil nesli taştı, tombstone korunuyor/,
    "u32::MAX must refuse, not wrap",
  );
  // The owner is part of the durable record, because a name cannot be reversed
  // into the site id that derives it.
  assert.match(windowsRs, /pub enum PendingOwner/);
  assert.match(windowsRs, /pub struct PendingProfileDelete/);
  assert.match(windowsRs, /pub fn match_queued_name_to_site/);
  assert.match(windowsRs, /pub fn generation_past_name/);
  assert.match(windowsRs, /pub fn secure_generation_past_name/);
  // Backward compatibility: the older names-only queue is still read.
  assert.match(windowsRs, /serde_json::from_slice::<Vec<String>>\(raw\)/);
  assert.match(windowsRs, /pub fn recover_pending_profile_delete_entries/);
  // lib.rs delegates instead of re-implementing the order (where no host test
  // could reach it), and re-matches ownerless tombstones against the registry.
  const libRs = read("src-tauri/src/lib.rs");
  const drain = libRs.slice(libRs.indexOf("fn drain_pending_profile_deletes"));
  assert.match(drain, /w::run_pending_delete_drain\(/);
  assert.match(drain, /w::read_pending_profile_delete_entries\(state\)/);
  assert.match(drain, /w::match_queued_name_to_site\(name, &site_ids\)/);
  assert.match(drain, /report\.kept_unmatched/);
  assert.doesNotMatch(
    code(drain),
    /forget_pending_profile_delete/,
    "the drain order lives in ONE host-tested function, not in lib.rs",
  );
  assert.doesNotMatch(code(drain), /site_profile_retire\(name\)/);
  // The third deletion path — the "clear all" orphan sweep — must run the SAME
  // order as the other two, and that order has to live where `cargo test` can
  // see it: the sweep block in commands.rs is #[cfg(target_os = "android")] and
  // is only type-checked by the aarch64 job.
  const sweepEntryAt = windowsRs.indexOf("pub fn run_orphan_profile_sweep");
  const sweepAt = windowsRs.indexOf("fn run_orphan_profile_sweep_inner");
  assert.ok(sweepEntryAt > 0 && sweepAt > sweepEntryAt, "the host-tested orphan sweep is missing");
  assert.match(
    windowsRs.slice(sweepEntryAt, sweepAt),
    /pub fn run_orphan_profile_sweep<D, R>\(/,
    "the public sweep entry takes injected provider calls",
  );
  const sweepFn = windowsRs.slice(
    sweepAt,
    windowsRs.indexOf("/// Android: open the site in a fresh native WebView"),
  );
  assert.ok(sweepFn.length > 500, "the host-tested orphan sweep body is missing");
  const tombstoneAt = sweepFn.indexOf("queue_orphan_profile_delete(state, name)");
  const deleteAt = sweepFn.indexOf("delete_profile(name)");
  const blockAt = sweepFn.indexOf("block_pending_profile_delete(state, name)");
  const retireAt = sweepFn.indexOf("retire_profile(name)");
  assert.ok(tombstoneAt > 0, "the sweep must write the durable tombstone");
  assert.ok(deleteAt > tombstoneAt, "the provider call must start AFTER the tombstone is durable");
  assert.ok(blockAt > deleteAt, "the permanent reuse block must follow a confirmed deletion");
  assert.ok(retireAt > blockAt, "the quarantine ledger is retired LAST");
  assert.doesNotMatch(
    code(sweepFn),
    /forget_pending_profile_delete/,
    "no path in the sweep may drop the durable record of a confirmed deletion",
  );
  // The durable block itself: a confirmed orphan has no site id, so there is no
  // generation to secure and the queue entry becomes the block the open
  // protocol consults.
  assert.match(
    windowsRs,
    /pub fn block_pending_profile_delete\(state: &AppState, name: &str\) -> AppResult<\(\)>/,
  );
  assert.match(windowsRs, /pub enum PendingDeleteState \{/);
  assert.match(windowsRs, /#\[serde\(default\)\]\s+pub state: PendingDeleteState,/);
  assert.match(windowsRs, /pub blocked: usize,/);
  // The drain consults the block: a blocked entry is not re-deleted, and an
  // orphan without an owner is held, never lifted.
  const drainInnerAt = windowsRs.indexOf("fn run_pending_delete_drain_inner");
  const drainInner = windowsRs.slice(drainInnerAt, windowsRs.indexOf("/// What one \"clear all\" orphan sweep did.", drainInnerAt));
  const blockedAt = drainInner.indexOf("let action = if entry.is_blocked()");
  const blockedElseAt = drainInner.indexOf("} else {", blockedAt);
  assert.ok(blockedAt > 0 && blockedElseAt > blockedAt, "blocked-state branch is missing");
  const blockedBranch = drainInner.slice(blockedAt, blockedElseAt);
  assert.match(blockedBranch, /SiteProfileAction::Absent/);
  assert.doesNotMatch(blockedBranch, /delete_profile\(/,
    "a permanent block must not be sent to the provider again");
  assert.match(windowsRs, /None => hold_as_block\(state, entry, &mut report\),/);
  const holdFn = windowsRs.slice(
    windowsRs.indexOf("fn hold_as_block("),
    windowsRs.indexOf("/// Drain the deferred profile-deletion queue"),
  );
  assert.ok(
    holdFn.length > 200 && holdFn.length < 4000,
    `hold_as_block slice looks wrong: ${holdFn.length}`,
  );
  assert.match(
    code(holdFn).trimEnd(),
    /false\s*\}\s*$/,
    "hold_as_block returns false on EVERY path: an unmatched orphan is never lifted",
  );
  assert.match(holdFn, /report\.blocked \+= 1;/);
  assert.match(
    holdFn,
    /report\.kept_unmatched\.push\(entry\.name\.clone\(\)\);/,
    "a refused block write is reported, not swallowed",
  );
  assert.doesNotMatch(
    code(holdFn),
    /forget_pending_profile_delete/,
    "holding a block must not drop the queue entry",
  );

  // commands.rs only decides WHICH names are orphans; it must not call the
  // provider or write the tombstone itself, or the order would be untestable.
  const commandsRs = read("src-tauri/src/commands.rs");
  const sweep = commandsRs.slice(commandsRs.indexOf('orphan sweep for "clear all"'));
  assert.match(sweep, /windows::run_orphan_profile_sweep\(/);
  assert.match(
    sweep,
    /crate::android_bridge::site_profile_delete,\s+crate::android_bridge::site_profile_retire,/,
    "the sweep is handed the real provider calls, not a reordered copy",
  );
  assert.doesNotMatch(
    code(sweep),
    /site_profile_delete\(&name\)/,
    "the orphan sweep must not call the provider directly",
  );
  assert.doesNotMatch(
    code(sweep),
    /queue_orphan_profile_delete/,
    "the tombstone write belongs to the host-tested sweep",
  );
});

// ---------------------------------------------------------------------------
// Window/back/PiP regression surface
// ---------------------------------------------------------------------------

test("the Android open path never navigates the launcher or evals the overlay", () => {
  const mobileStart = windowsRs.indexOf("fn open_on_mobile(");
  assert.ok(mobileStart > 0);
  const body = windowsRs.slice(mobileStart, windowsRs.indexOf("\n}\n", mobileStart));
  assert.doesNotMatch(body, /OVERLAY_SNIPPET/);
  assert.doesNotMatch(body, /window\.navigate/);
  assert.doesNotMatch(body, /clear_all_browsing_data/);
  assert.match(
    body,
    /run_mobile_open_protocol\(/,
    "the site view is opened through the two-phase profiled protocol",
  );
});

test("the open is two-phased: site_migrate carries the jar, site_open carries no cookies", () => {
  // Plugin crate: distinct commands; the phase-2 payload cannot migrate.
  assert.match(pluginLibRs, /pub const CMD_SITE_MIGRATE: &str = "site_migrate"/);
  assert.match(pluginLibRs, /pub fn site_migrate\(/);
  const openPayloadAt = pluginLibRs.indexOf("struct SiteOpenPayload");
  assert.ok(openPayloadAt > 0);
  const openPayload = pluginLibRs.slice(openPayloadAt, pluginLibRs.indexOf("}", openPayloadAt));
  assert.doesNotMatch(openPayload, /cookies/, "phase 2 must not be able to migrate behind the flag");
  const migPayloadAt = pluginLibRs.indexOf("struct SiteMigratePayload");
  assert.ok(migPayloadAt > 0);
  assert.match(
    pluginLibRs.slice(migPayloadAt, pluginLibRs.indexOf("}", migPayloadAt)),
    /cookies: Vec<String>/,
  );
  // Bridge: both phases exist and neither carries the other's job.
  assert.match(
    bridgeRs,
    /pub fn site_migrate\(url: &str, profile: &str, cookies: &\[String\]\) -> AppResult<\(\)>/,
  );
  assert.match(
    bridgeRs,
    /pub fn site_open\(url: &str, profile: &str, init_script: &str\) -> AppResult<\(\)>/,
  );
  // Kotlin: an @Command per phase; site_open never parses a cookie array.
  assert.match(pluginKt, /fun site_migrate\(invoke: Invoke\)/);
  assert.match(pluginKt, /AnimeHubSiteController\.migrate\(activity, url, profileName, cookies, operationId\)/);
  assert.match(pluginKt, /AnimeHubSiteController\.open\(activity, url, profileName, initScript, operationId\)/);
  const siteOpenAt = pluginKt.indexOf("fun site_open(invoke: Invoke)");
  assert.ok(siteOpenAt > 0);
  const siteOpenBody = pluginKt.slice(siteOpenAt, pluginKt.indexOf("\n  }\n", siteOpenAt));
  assert.doesNotMatch(siteOpenBody, /cookies/i, "the phase-2 command never sees the jar");
});

test("every awaited flush is watchdog-bounded, in close and clear alike", () => {
  const ops = pureKotlin;
  // Close: the port exposes a watchdog and the flow bounds the flush await.
  const closePortAt = ops.indexOf("interface SiteClosePort");
  assert.ok(closePortAt > 0);
  const closePort = ops.slice(closePortAt, ops.indexOf("\n}\n", closePortAt));
  assert.match(closePort, /fun scheduleTimeout\(millis: Long, onTimeout: \(\) -> Unit\)/);
  const closeFlowAt = ops.indexOf("object SiteCloseFlow");
  const closeFlow = ops.slice(closeFlowAt, ops.indexOf("\n}\n", closeFlowAt));
  assert.match(closeFlow, /const val FLUSH_TIMEOUT_MS: Long = 15_000/);
  assert.match(closeFlow, /port\.scheduleTimeout\(FLUSH_TIMEOUT_MS\)/);
  assert.match(
    closeFlow,
    /if \(!settled\.compareAndSet\(false, true\)\) \{[\s\S]*?return@flushCookies/,
    "a late flush callback must lose the race and return without destroying the view",
  );
  // The view is destroyed only by the settled winner, after the guard.
  const flushCbAt = closeFlow.indexOf("port.flushCookies(profileName) { error ->");
  assert.ok(flushCbAt > 0);
  const flushCb = closeFlow.slice(flushCbAt);
  assert.ok(
    flushCb.indexOf("settled.compareAndSet") < flushCb.indexOf("port.destroyActiveView()"),
    "destroy must sit behind the once-guard",
  );
  // Clear: the FINAL flush is bounded too (it used to be a naked await).
  const clearFlowAt = ops.indexOf("object SiteClearFlow");
  const clearFlow = ops.slice(clearFlowAt, ops.indexOf("\n}\n", clearFlowAt));
  assert.match(clearFlow, /const val FLUSH_TIMEOUT_MS: Long = 15_000/);
  assert.match(clearFlow, /port\.scheduleTimeout\(FLUSH_TIMEOUT_MS\)/);
  assert.match(
    clearFlow,
    /if \(!flushSettled\.compareAndSet\(false, true\)\) \{[\s\S]*?return@flushCookies/,
  );
  // Migration keeps its own bounds.
  assert.match(ops, /const val COOKIE_TIMEOUT_MS: Long = 10_000/);
  // The controller implements the close watchdog over the main looper.
  const closeAt = controllerKt.indexOf("fun close(activity: Activity, flush: Boolean");
  assert.ok(closeAt > 0);
  const closeBody = controllerKt.slice(closeAt, controllerKt.indexOf("SiteCloseFlow.run", closeAt));
  assert.match(
    closeBody,
    /override fun scheduleTimeout\(millis: Long, onTimeout: \(\) -> Unit\): Watchdog/,
  );
  // The adapters delegate to ONE helper, which posts on the main looper and
  // returns a removeCallbacks-based canceller (see the watchdog test below).
  assert.match(closeBody, /SiteFlushWorker\.scheduleWatchdog\(millis, onTimeout\)/);
  const helperAt = flushWorkerKt.indexOf("fun scheduleWatchdog(");
  assert.ok(helperAt > 0, "the watchdog helper lives in SiteFlushWorker.kt");
  const helper = flushWorkerKt.slice(helperAt, flushWorkerKt.indexOf("\n  }\n", helperAt));
  assert.match(helper, /mainHandler\.postDelayed\(task, millis\)/);
  assert.match(helper, /mainHandler\.removeCallbacks\(task\)/);
  // The doc's "every awaited step has a watchdog" claim is now backed by the
  // flush constants it names.
  assert.match(read("docs/android-storage-isolation.md"), /FLUSH_TIMEOUT_MS/);
});

test("a timed-out wipe cannot mutate the profile behind a new session", () => {
  // Static/contract half of the guard: the BEHAVIOUR (zero `geo.clear` calls
  // after a watchdog, the quarantine gate, the late-answer release) is proven
  // by SiteProfileOpsTest, which drives the production GeolocationWipe and
  // SiteClearFlow with a recording provider fake. This test pins the wiring
  // that makes those guarantees reach the real adapter.
  const ops = pureKotlin;

  // The awaited geolocation step receives a validity token, and the pure
  // production wipe claims it BEFORE the first mutation.
  assert.match(
    ops,
    /fun clearGeolocationPermissions\(guard: StepGuard, onDone: \(Boolean\) -> Unit\)/,
  );
  const wipeAt = ops.indexOf("object GeolocationWipe");
  assert.ok(wipeAt > 0, "the wipe sequencing must be pure, CI-executed code");
  const wipe = ops.slice(wipeAt, ops.indexOf("\n}\n", wipeAt));
  const claim = wipe.indexOf("guard.claim()");
  const mutate = wipe.indexOf("api.clear(origin)");
  assert.ok(claim > 0, "the wipe must claim the step guard");
  assert.ok(mutate > claim, "the claim must precede the mutation");
  assert.match(wipe, /if \(!guard\.claim\(\)\) return@getOrigins/);
  assert.match(ops, /interface StepGuard/);
  assert.match(ops, /fun claim\(\): Boolean/);

  // The real adapter delegates instead of clearing origins inline.
  assert.match(controllerKt, /GeolocationWipe\.start\(/);
  assert.doesNotMatch(controllerKt, /for \(origin in origins\) geo\.clear\(origin\)/);
  assert.match(
    controllerKt,
    /SiteClearFlow\.run\(supported, profileName, port, operationId\)/,
    "the flow needs the name to quarantine the right profile",
  );

  // The destructive call that CANNOT be gated is registered under its OWN
  // operation id, and BOTH open phases consult the gate before any provider call.
  assert.match(ops, /object ProfileQuarantine/);
  assert.match(ops, /object SiteOpenGate/);
  assert.match(
    ops,
    /ProfileQuarantine\.quarantine\(profileName, guard\.operationId, COOKIE_UNVERIFIED_REASON\)/,
    "an unverified removeAllCookies must register its own operation",
  );
  assert.match(
    ops,
    /ProfileQuarantine\.settle\(profileName, guard\.operationId\)/,
    "a late callback may close only the operation it belongs to",
  );
  assert.match(
    ops,
    /ProfileQuarantine\.verified\(profileName\)/,
    "a verified wipe records a verification instead of releasing the name",
  );
  // A blanket release/reset would erase other operations' records.
  assert.doesNotMatch(ops, /ProfileQuarantine\.release\(/);
  assert.doesNotMatch(ops, /ProfileQuarantine\.reset\(\)/);
  assert.doesNotMatch(controllerKt, /ProfileQuarantine\.(release|reset)\(/);
  // Ownership ledger: reopenability is a proven end state, not a counter.
  assert.match(ops, /val pending = LinkedHashMap<Long, String>\(\)/);
  assert.match(ops, /STALE_VERIFICATION_REASON/);
  assert.match(ops, /fun settle\(profileName: String, operationId: Long\): Boolean/);
  assert.match(ops, /fun retire\(profileName: String\): Int/);
  assert.match(ops, /fun pendingOperations\(profileName: String\): List<Long>/);
  assert.match(
    ops,
    /if \(state\.verifiedAt < state\.lastSettledAt\) return STALE_VERIFICATION_REASON/,
    "the last verification must postdate the last settled operation",
  );
  for (const entry of ["fun migrate(", "fun open("]) {
    const at = controllerKt.indexOf(entry);
    assert.ok(at > 0, `${entry} missing`);
    const body = controllerKt.slice(at, controllerKt.indexOf("activity.runOnUiThread", at));
    assert.match(
      body,
      /quarantineRefusal\(profileName, /,
      `${entry} must refuse a blocked profile before any provider call`,
    );
    assert.match(
      body,
      /SiteOpenGate\.check|quarantineRefusal\(/,
      `${entry} must consult the same gate`,
    );
  }
  // Retirement is the ONLY path that may drop outstanding records, and it is a
  // separate entry point that Rust sequences AFTER the durable generation bump
  // (see the delete-protocol test above): a provider merely *accepting* the
  // deletion is not proof the name is unreachable, so `deleteProfileData` must
  // not retire anything itself.
  const deleteAt = controllerKt.indexOf("fun deleteProfileData");
  const deleteBody = code(controllerKt.slice(deleteAt, controllerKt.indexOf("\n  }\n", deleteAt)));
  assert.doesNotMatch(deleteBody, /ProfileQuarantine\.retire\(/);
  const retireAt = controllerKt.indexOf("fun retireProfileQuarantine");
  assert.ok(retireAt > deleteAt, "retirement is its own entry point");
  const retireBody = controllerKt.slice(retireAt, controllerKt.indexOf("\n  }\n", retireAt));
  assert.match(retireBody, /ProfileQuarantine\.retire\(profileName\)/);
  assert.match(controllerKt, /ProfileQuarantine\.isBlocked\(profileName\)/);
  const gateAt = ops.indexOf("object SiteOpenGate");
  const gate = ops.slice(gateAt, ops.indexOf("\n}\n", gateAt));
  assert.match(
    gate,
    /ProfileQuarantine\.reasonFor\(profileName\)/,
    "one gate reads the ledger, and both open phases call that gate",
  );

  // Rust maps the sentinel onto guidance instead of showing it raw, and keeps
  // it distinct from the migration-retry retirement path.
  assert.match(
    pluginLibRs,
    /pub const QUARANTINE_SENTINEL: &str = "profile-quarantined"/,
  );
  assert.match(pluginLibRs, /pub fn is_profile_quarantined\(/);
  assert.match(bridgeRs, /pub const PROFILE_QUARANTINED_MSG: &str = "/);
  assert.match(
    bridgeRs,
    /AppError::Quarantined\(PROFILE_QUARANTINED_MSG\.to_string\(\)\)/,
  );
  assert.equal(
    (bridgeRs.match(/classify_site_open_error\(error\)/g) || []).length,
    2,
    "both open phases classify their refusals the same way",
  );
});

test("the cookie step reads a TYPED delivery: an empty jar succeeds, an unreachable store does not", () => {
  const ops = pureKotlin;
  // Chromium's `removeAllCookies` callback reports whether ANY cookie was
  // deleted, so `false` over an empty jar is a successful wipe — and `false`
  // from a store that could not be reached is an error. One boolean cannot say
  // both, so the port carries a type and exactly one pure function reads it.
  assert.match(ops, /sealed interface CookieWipe/);
  assert.match(ops, /data class Answered\(val deleted: Boolean\) : CookieWipe/);
  assert.match(ops, /data class Failed\(val reason: String\) : CookieWipe/);
  assert.match(ops, /fun removeAllCookies\(onDone: \(CookieWipe\) -> Unit\)/);
  assert.match(ops, /fun cookiesCleared\(\): Boolean\?/);
  assert.doesNotMatch(
    ops,
    /fun removeAllCookies\(onDone: \(Boolean\) -> Unit\)/,
    "the boolean-shaped cookie port must not come back",
  );
  assert.match(
    ops,
    /fun interpretCookieWipe\(wipe: CookieWipe, cookiesCleared: \(\) -> Boolean\?\): CookieStep/,
  );
  // An empty jar is a success; cookies still present, or a store whose emptiness
  // cannot be read, are failures — never a silent success.
  const interpAt = ops.indexOf("fun interpretCookieWipe(");
  const interp = ops.slice(interpAt, ops.indexOf("\n  }\n", interpAt));
  assert.match(interp, /if \(wipe\.deleted\)/);
  assert.match(interp, /true -> CookieStep\(true, null\)/);
  assert.match(interp, /false -> CookieStep\(false, COOKIE_STILL_PRESENT_REASON\)/);
  assert.match(interp, /null -> CookieStep\(false, COOKIE_STATE_UNKNOWN_REASON\)/);
  assert.match(interp, /is CookieWipe\.Failed -> CookieStep\(false,/);
  // The emptiness probe is the tie-breaker for `deleted == false` ONLY, so it
  // never races a deletion that just completed.
  assert.ok(
    interp.indexOf("wipe.deleted") < interp.indexOf("cookiesCleared()"),
    "the probe runs inside the indeterminate branch",
  );
  // The adapter hands the raw answer through and keeps real errors typed — via
  // the PURE mapping the CI unit tests execute, because two of its rules are
  // inversion-sensitive: a null delivery is not a confirmed deletion, and
  // `hasCookies()` asks the opposite of "is the jar empty?".
  assert.match(ops, /object CookieWipes/);
  assert.match(
    ops,
    /fun answered\(deleted: Boolean\?\): CookieWipe = CookieWipe\.Answered\(deleted == true\)/,
  );
  assert.match(ops, /fun noProfile\(\): CookieWipe = CookieWipe\.Failed\(NO_PROFILE_REASON\)/);
  assert.match(ops, /runCatching \{ !hasCookies\(\) \}\.getOrNull\(\)/);
  assert.match(controllerKt, /onDone\(CookieWipes\.answered\(deleted\)\)/);
  assert.match(controllerKt, /onDone\(CookieWipes\.failed\(e\)\)/);
  assert.match(controllerKt, /onDone\(CookieWipes\.noProfile\(\)\)/);
  assert.doesNotMatch(
    controllerKt,
    /onDone\(ok == true\)/,
    "the adapter must not collapse the provider answer into a success boolean",
  );
  assert.match(controllerKt, /override fun cookiesCleared\(\): Boolean\?/);
  assert.match(
    controllerKt,
    /CookieWipes\.emptinessOf \{ profile\.cookieManager\.hasCookies\(\) \}/,
    "the probe must go through the tested inversion of hasCookies()",
  );
  // Round 6 is untouched by this: the timeout still registers its own operation.
  assert.match(
    ops,
    /ProfileQuarantine\.quarantine\(profileName, guard\.operationId, COOKIE_UNVERIFIED_REASON\)/,
  );
  assert.match(ops, /ProfileQuarantine\.settle\(profileName, guard\.operationId\)/);
});

test("every awaited step cancels its watchdog, and an unarmable one fails fast", () => {
  const ops = pureKotlin;
  const opsTest = read(
    "src-tauri/android-plugin/kotlin/dev/animehub/app/SiteProfileOpsTest.kt",
  );
  // All three ports hand back a canceller...
  assert.equal(
    (ops.match(/fun scheduleTimeout\(millis: Long, onTimeout: \(\) -> Unit\): Watchdog/g) || [])
      .length,
    3,
    "close, clear and migration ports report armed/refused explicitly",
  );
  // ...and every arming site has a matching cancellation once the step settles.
  // Without it a step that answered normally left its delayed runnable queued
  // for the whole window (10 s per step, 15 s per flush), holding the port —
  // and through it the site WebView and the Activity — reachable.
  assert.equal((ops.match(/port\.scheduleTimeout\(/g) || []).length, 5);
  assert.equal((ops.match(/cancel\w*Watchdog\(\)/g) || []).length, 6);
  assert.match(ops, /val cancelWatchdog = \(watchdog as Watchdog\.Armed\)\.cancel/);
  assert.match(ops, /val cancelFlushWatchdog = \(flushWatchdog as Watchdog\.Armed\)\.cancel/);
  assert.match(ops, /val cancelCookieWatchdog = \(cookieWatchdog as Watchdog\.Armed\)\.cancel/);

  // ONE production helper arms watchdogs: removeCallbacks-based cancellation,
  // and an immediate failure when the handler refuses the post (a flow whose
  // watchdog never fires and whose provider never answers would otherwise leave
  // the caller — and the bridge call behind it — waiting forever).
  assert.match(
    flushWorkerKt,
    /fun scheduleWatchdog\(millis: Long, onTimeout: \(\) -> Unit\): Watchdog/,
  );
  // postDelayed's RETURN VALUE is the refusal channel (it throws nothing), so
  // it must be captured and fed to the pure decision.
  assert.match(flushWorkerKt, /queued = mainHandler\.postDelayed\(task, millis\)/);
  assert.match(flushWorkerKt, /WatchdogArming\.refusal\(queued, thrown\)/);
  assert.match(flushWorkerKt, /return Watchdog\.Refused/);
  assert.match(flushWorkerKt, /Watchdog\.Armed\(\{ mainHandler\.removeCallbacks\(task\) \}\)/);
  assert.match(
    pureKotlin,
    /if \(thrown != null \|\| !queued\) "watchdog_refused" else null/,
    "the pure decision must use a safe refusal code for both false and thrown posts",
  );
  // On refusal the worker must NOT settle the step itself: the flow owns that,
  // and firing onTimeout here would report a timeout for a provider call that
  // was never issued (and quarantine an operation that does not exist).
  const refusalBranch = flushWorkerKt.slice(
    flushWorkerKt.indexOf("if (refusal != null)"),
    flushWorkerKt.indexOf("return Watchdog.Armed"),
  );
  assert.doesNotMatch(code(refusalBranch), /onTimeout\(\)/);
  assert.equal(
    (controllerKt.match(/SiteFlushWorker\.scheduleWatchdog\(millis, onTimeout\)/g) || []).length,
    3,
    "migration, close and clear adapters all share the one helper",
  );
  assert.doesNotMatch(
    code(controllerKt),
    /postDelayed\(\{ onTimeout\(\) \}, millis\)/,
    "the un-cancellable inline post must be gone",
  );

  // The contract is executed, not just written down.
  for (const name of [
    "a settled clear cancels every watchdog it armed",
    "the watchdog that fired is the only one a timed-out clear leaves queued",
    "a refused watchdog fails the step and never starts the provider call",
    "a refusal of only the final flush skips the flush and fails the wipe",
    "WatchdogArming treats a false postDelayed exactly like a throw",
    "a refused close watchdog keeps the view and never starts the flush",
    "a refused migration watchdog writes no cookie and claims no success",
    "a settled migration cancels its per-cookie and flush watchdogs",
  ]) {
    assert.ok(opsTest.includes(name), `missing Kotlin case: ${name}`);
  }
});

test("a refused watchdog stops the flow before any provider work starts", () => {
  const close = pureFile("SiteCloseFlow.kt");
  const clear = pureFile("SiteClearFlow.kt");
  const migration = pureFile("SiteMigrationFlow.kt");
  // Each arming site decides on Watchdog.Refused BEFORE the call it would bound.
  assert.ok(
    close.indexOf("is Watchdog.Refused") < close.indexOf("port.flushCookies(profileName)"),
    "close must not start an unbounded blocking flush",
  );
  assert.ok(
    clear.indexOf("is Watchdog.Refused") < clear.indexOf("arm(guard)"),
    "an awaited clear step must not arm its provider call without a watchdog",
  );
  assert.ok(
    clear.indexOf("flushWatchdog is Watchdog.Refused") < clear.lastIndexOf("port.flushCookies"),
    "the clear's final flush must not start unbounded",
  );
  assert.ok(
    migration.indexOf("cookieWatchdog is Watchdog.Refused") < migration.indexOf("port.setCookie("),
    "a migration cookie must not be written without a bound",
  );
  assert.ok(
    migration.indexOf("flushWatchdog is Watchdog.Refused") < migration.indexOf("port.flushCookies"),
    "a migration flush must not start unbounded",
  );
  // The refusal is its own reported outcome, distinct from a timeout, and the
  // timeout hook (which quarantines an outstanding cookie deletion) is NOT run
  // for it — nothing was issued, so nothing is outstanding.
  assert.match(pureKotlin, /const val WATCHDOG_REFUSED: String = "zamanlayıcı kurulamadı"/);
  const refusedAt = clear.indexOf("if (watchdog is Watchdog.Refused)");
  assert.ok(refusedAt > 0);
  // Only the refusal branch: the timeout lambda above it legitimately runs the
  // hook, because there a deletion really was issued and is now unverified.
  const refusedBranch = clear.slice(refusedAt, clear.indexOf("arm(guard)", refusedAt));
  assert.match(refusedBranch, /SiteProfileErrors\.WATCHDOG_REFUSED/);
  assert.doesNotMatch(
    code(refusedBranch),
    /onTimeout\(guard\)/,
    "a refusal must not quarantine an operation that was never issued",
  );
  assert.match(
    refusedBranch,
    /no provider call is issued and there is no\s*\/\/ destructive operation to quarantine/,
    "the branch must explain that a refused watchdog leaves no provider operation outstanding",
  );
  assert.match(refusedBranch, /next\(false\)/, "the clear reports failure without starting provider work");
  // And the timeout path still quarantines (unchanged contract).
  assert.match(clear, /ProfileQuarantine\.quarantine\(profileName, guard\.operationId, COOKIE_UNVERIFIED_REASON\)/);
});

test("all blocking flushes share ONE worker, with stuck detection not new threads", () => {
  // Every AWAITED flush (migration, close, clear, the periodic sync) routes
  // through submitFlush — the single place that answers a caller and can
  // therefore notice a stuck worker.
  assert.match(flushWorkerKt, /object SiteFlushWorker/);
  assert.match(flushWorkerKt, /fun submit\(\s*activity: Activity,/);
  assert.equal(
    (controllerKt.match(/SiteFlushWorker\.submit\(activity, "/g) || []).length,
    4,
    "migration, close, clear and the periodic sync",
  );
  // The only other submission is destroy's flush. It runs on that SAME single
  // worker — no extra threads — and is awaited for a bounded budget, because
  // the generated Activity kills the process right after teardown: a
  // fire-and-forget flush there could be killed mid-write.
  // ONE executor factory, in ONE file: recovery creates a replacement through
  // the same factory, and the controller may never queue work behind its back.
  assert.match(flushWorkerKt, /private fun newFlushExecutor\(\): ExecutorService/);
  assert.match(flushWorkerKt, /flushExecutor = newFlushExecutor\(\)/);
  assert.match(flushWorkerKt, /FLUSH_STUCK_REPLACE_MS/);
  assert.match(flushWorkerKt, /shutdownNow\(\)/);
  assert.match(flushWorkerKt, /flush worker replaced after a stuck provider call/);
  assert.match(pureKotlin, /object FlushRecovery/);
  assert.doesNotMatch(code(controllerKt), /newFlushExecutor|flushExecutor/);
  assert.equal(
    (controllerKt.match(/SiteFlushWorker\.execute\("destroy", operationId\)/g) || []).length,
    1,
    "the destroy-time flush uses the same worker",
  );
  const destroyAt = controllerKt.indexOf("fun destroy(activity: Activity, operationId: String");
  assert.ok(destroyAt > 0);
  const destroyBody = controllerKt.slice(
    destroyAt,
    controllerKt.indexOf("private fun ensureBackCallback", destroyAt),
  );
  assert.match(destroyBody, /SiteFlushWorker\.execute\("destroy", operationId\)/);
  assert.match(
    destroyBody,
    /ShutdownFlow\.run\(/,
    "the destroy-time flush is awaited within a budget, not fire-and-forget",
  );
  const teardownDoc = controllerKt.slice(
    controllerKt.indexOf("Activity teardown hands"),
    destroyAt,
  );
  assert.match(teardownDoc, /BOUNDED budget/, "documented as a bounded await");
  assert.doesNotMatch(
    controllerKt,
    /never awaited/,
    "the fire-and-forget teardown claim must be gone",
  );
  // A watchdog answers the caller; it cannot cancel the blocking provider
  // call, so a stuck worker must be observable instead of hidden.
  assert.match(flushWorkerKt, /FLUSH_STUCK_WARN_MS/);
  assert.match(flushWorkerKt, /flush worker remains busy/);
  assert.equal(
    (flushWorkerKt.match(/Executors\.newSingleThreadExecutor/g) || []).length,
    1,
  );
  for (const src of [controllerKt, flushWorkerKt, pureKotlin]) {
    assert.doesNotMatch(
      src,
      /newCachedThreadPool|newFixedThreadPool|newWorkStealingPool/,
      "no thread pool inflation to dodge a stuck flush",
    );
  }
});

test("back navigation: fullscreen first, then site history, then the launcher hook", () => {
  assert.match(controllerKt, /if \(customView != null\) \{\s*exitCustomViewIfNeeded\(\)/);
  assert.match(controllerKt, /webView\.canGoBack\(\)/);
  assert.match(controllerKt, /webView\.goBack\(\)/);
  assert.match(controllerKt, /closeViaLauncher\(activity, operationId\)/);
  // The callback is disabled when no site is open, so the launcher's own
  // default back behaviour (Tauri: no wry callback) stays as before.
  assert.match(controllerKt, /private var backCallback: OnBackPressedCallback\?/);
  assert.match(controllerKt, /backCallback\?\.isEnabled = false/);
  // The launcher-side hook returns whether a close session ACCEPTED the
  // request (never a bare "it ran"), and exposes the status Kotlin polls
  // for the real outcome (pending | closed | failed:<reason> | idle).
  assert.match(
    mainJs,
    /window\.__animehubBackToLauncher = \(operationId\) => backSession\.requestClose\(operationId\)\.accepted;/,
  );
  assert.match(mainJs, /window\.__animehubBackStatus = \(\) => backSession\.statusString\(\);/);
  // A.5: the label must not be dropped in a finally after invoking close.
  assert.doesNotMatch(mainJs, /finally \{\s*\n\s*openSiteLabel = null;/);
  // No site-facing close-scheme ever appears on Android.
  assert.doesNotMatch(controllerKt, /animehub:\/\/close-site/);
});

test("PiP re-binds between the site WebView and the launcher", () => {
  // Open: bind to the site view. Close/failure/destroy: use the shared,
  // behavior-tested launcher-transfer flow rather than duplicating attach logic.
  assert.match(controllerKt, /AnimeHubPipController\.attach\(activity, webView, operationId\)/);
  const launcherTransfers =
    controllerKt.match(/transferPipToLauncher\(activity, operationId\)/g) || [];
  assert.ok(
    launcherTransfers.length >= 2,
    "close and failure paths must re-bind to the launcher",
  );
  assert.match(controllerKt, /PipTransferFlow\.transferToLauncher\(/);
  // The plugin remembers the launcher at load time for both purposes.
  assert.match(pluginKt, /AnimeHubSiteController\.bindLauncher\(webView\)/);
  // Activity teardown orders site-destroy before PiP detach.
  assert.match(preparePy, /AnimeHubSiteController\.destroy\(this\)/);
  assert.match(preparePy, /AnimeHubSiteController\.onPause\(this\)/);
  assert.match(preparePy, /AnimeHubSiteController\.onResume\(this\)/);
  const destroyAt = preparePy.indexOf("AnimeHubSiteController.destroy(this)");
  const detachAt = preparePy.indexOf("AnimeHubPipController.detach(this)");
  assert.ok(destroyAt > 0 && destroyAt < detachAt, "site view must go before PiP unbind");
});

test("the site WebView has no page-facing IPC surface", () => {
  for (const [name, src] of [
    ["controller", controllerKt],
    ["plugin", pluginKt],
    ["pipeline", pipKt],
  ]) {
    assert.doesNotMatch(src, /@JavascriptInterface/, `${name} must not expose a JS interface`);
    assert.doesNotMatch(src, /addJavascriptInterface/, `${name} must not add a JS interface`);
  }
  // The site client is a plain client behind the policy guard — never wry's
  // RustWebViewClient (which carries the launcher IPC plumbing).
  assert.match(controllerKt, /SiteNavigationGuard\(SiteWebViewClient\(/);
  assert.doesNotMatch(code(controllerKt), /RustWebViewClient|RustWebView\(/);
});

test("the site WebView settings come from a tested plan, not inline defaults", () => {
  // AH-06 / AH-P1 / AH-P3. The values themselves are unit-tested in
  // SiteWebViewSettingsTest through the real applySiteSettings; these pins hold
  // the wiring: buildWebView applies the plan, the inline assignments are gone,
  // and the file that decides them stays pure and packaged.
  const settingsKt = read(`${KOTLIN}/SiteWebViewSettings.kt`);
  const buildAt = controllerKt.indexOf("private fun buildWebView(");
  assert.ok(buildAt > 0, "buildWebView missing");
  const applyAt = controllerKt.indexOf("applySiteSettings(WebSettingsSink(webView.settings))", buildAt);
  assert.ok(applyAt > buildAt, "buildWebView must apply the plan");
  const setProfile = controllerKt.indexOf("WebViewCompat.setProfile(webView, profileName)", buildAt);
  const addView = controllerKt.indexOf("content.addView(", buildAt);
  const loadUrl = controllerKt.indexOf("webView.loadUrl(url)", buildAt);
  assert.ok(setProfile < applyAt, "settings come after setProfile");
  assert.ok(applyAt < addView && applyAt < loadUrl, "settings come before attach and navigation");

  // No setting is assigned inline in buildWebView any more: the plan is the
  // single source of truth (a stray `webView.settings.x = …` would bypass it).
  const body = code(controllerKt).slice(buildAt, addView);
  assert.doesNotMatch(body, /webView\.settings\.[A-Za-z]+\s*=/, "no inline settings assignment");
  assert.doesNotMatch(body, /setGeolocationEnabled\(true\)/, "geolocation is not enabled inline");

  // The plan is pure (compiled into the unit-test source set) and decides the
  // settings that the platform defaults leave unsafe.
  assert.equal(settingsKt.startsWith("// SPDX-License-Identifier: MIT"), true);
  assert.doesNotMatch(settingsKt, /^import android\./m, "the plan file must stay pure");
  assert.match(settingsKt, /val geolocationEnabled: Boolean = false/);
  assert.match(settingsKt, /val allowFileAccess: Boolean = false/);
  assert.match(settingsKt, /val allowContentAccess: Boolean = false/);
  assert.match(settingsKt, /val allowFileAccessFromFileURLs: Boolean = false/);
  assert.match(settingsKt, /val allowUniversalAccessFromFileURLs: Boolean = false/);
  assert.match(settingsKt, /val mixedContentMode: Int = MIXED_CONTENT_NEVER_ALLOW/);
  assert.match(settingsKt, /val supportMultipleWindows: Boolean = false/);
  // Playback and site logins must keep working (PiP/video non-regression).
  assert.match(settingsKt, /val mediaPlaybackRequiresUserGesture: Boolean = false/);
  assert.match(settingsKt, /val javaScriptEnabled: Boolean = true/);
  assert.match(settingsKt, /val domStorageEnabled: Boolean = true/);
  // 1 is NEVER_ALLOW; 0 would be ALWAYS_ALLOW and 2 COMPATIBILITY_MODE.
  assert.match(settingsKt, /const val MIXED_CONTENT_NEVER_ALLOW = 1/);
  // The adapter is the only place the platform object is written.
  assert.match(controllerKt, /private class WebSettingsSink\(private val s: WebSettings\) : SiteSettingsSink/);
  // And the new pure file has to be packaged, or the APK build breaks.
  assert.match(preparePy, /"SiteWebViewSettings"/);

  // No silent grants: the chrome client denies permission and geolocation
  // prompts instead of relying on the provider default.
  assert.match(controllerKt, /override fun onPermissionRequest\(request: PermissionRequest\)/);
  assert.match(controllerKt, /request\.deny\(\)/);
  assert.match(controllerKt, /override fun onGeolocationPermissionsShowPrompt\(/);
  assert.match(controllerKt, /callback\.invoke\(origin, false, false\)/);
  // Neither denial logs the origin.
  const chromeAt = controllerKt.indexOf("private class SiteChromeClient(");
  const chromeBody = controllerKt.slice(chromeAt, controllerKt.indexOf("private class WebSettingsSink"));
  assert.doesNotMatch(chromeBody, /AnimeHubDebugLog\.record\([^)]*origin/, "no origin in the denial log");
});

test("the developer log is gated and carries no host, url or token values", () => {
  // AH-P9. The launcher UI hides the panel when developerOptions is off, but
  // the command was callable by anything in the launcher WebView. Site WebViews
  // have no Tauri IPC at all, so this is defence in depth on the trusted side.
  const commands = read("src-tauri/src/commands.rs");
  const at = commands.indexOf("pub fn debug_log(");
  assert.ok(at > 0, "debug_log is missing");
  const body = commands.slice(at, commands.indexOf("\n}\n", at));
  assert.match(body, /developer_options/, "must consult the setting");
  assert.match(body, /return Ok\(crate::diagnostics::empty_snapshot\(\)\)/, "a structured empty snapshot, not an error");
  const gate = body.indexOf("if !enabled");
  const bridge = body.indexOf("crate::android_bridge::debug_log()");
  assert.ok(gate > 0 && bridge > gate, "the bridge is only reached when enabled");
  assert.match(body, /app: AppHandle/, "state comes from the handle");

  // Every Kotlin log line is an event, never a value from the page or the URL.
  for (const file of [
    "AnimeHubDebugLog.kt",
    "AnimeHubPlugin.kt",
    "AnimeHubPipController.kt",
    "AnimeHubSiteController.kt",
    "SiteBackStatus.kt",
    "SiteClearFlow.kt",
    "SiteCloseFlow.kt",
    "SiteFlushWorker.kt",
    "SiteMigrationFlow.kt",
    "SiteNavigationGuard.kt",
    "SiteNavigationPolicy.kt",
    "SiteProfileOps.kt",
    "SiteShutdownFlow.kt",
    "SiteWebViewSettings.kt",
  ]) {
    const src = code(read(`${KOTLIN}/${file}`));
    for (const raw of src.split("\n")) {
      if (!/AnimeHubDebugLog\.record\(|\bnote\(|Log\.[dweiv]\(/.test(raw)) continue;
      assert.doesNotMatch(
        raw,
        /\$\{?\s*(uri|url|request\.url|host|token|cookie|secret|origin)/i,
        `${file} logs a sensitive value: ${raw.trim()}`,
      );
    }
  }
});

test("the navigation guard preserves blocking without per-request diagnostics", () => {
  // Per-request/page logs can reveal site identity and create unbounded noise.
  // Keep the policy decisions and fail-closed responses, but do not log them.
  const guardKt = read(`${KOTLIN}/SiteNavigationGuard.kt`);
  const guardBody = code(guardKt);
  assert.match(guardBody, /SiteNavigationPolicy\.rejection\(uri\)/);
  assert.match(guardBody, /if \(blocked != null\) \{\s*return true/);
  assert.match(guardBody, /SiteNavigationPolicy\.subresourceRejection\(uri\)/);
  assert.match(guardBody, /if \(blocked != null\) \{\s*return blockedResponse\(\)/);
  assert.doesNotMatch(guardBody, /AnimeHubDebugLog\.record|Log\.[a-z]+\(/,
    "navigation must not emit per-request logs");
});

test("top-level data: is refused on both platforms while blob: playback is kept", () => {
  // AH-12 / AH-G8: a top-level data: document is a fake page with an opaque
  // origin. blob: stays allowed because MSE players navigate the main frame to
  // blob object URLs; data:image/… logos are sub-resources and unaffected.
  const rs = read("src-tauri/src/sites/url_policy.rs");
  const policyKt = read(`${KOTLIN}/SiteNavigationPolicy.kt`);
  assert.match(rs, /"data" => return Some\(NavBlock::DataNavigation\)/);
  assert.match(rs, /url\.path\(\) == "blank"/, "only about:blank survives");
  assert.match(rs, /"blob" => \{/, "blob stays a separate case");
  assert.match(rs, /inner\.starts_with\("http:\/\/"/, "a blob over http is a downgrade");
  assert.match(policyKt, /"data" -> DATA_NAVIGATION/);
  assert.match(policyKt, /if \(target\.opaquePart == "blank"\) null else UNKNOWN_ABOUT/);
  assert.match(policyKt, /startsWith\("http:\/\/"\)\) INSECURE_SCHEME else null/);
  // The two platforms must not keep separate vector lists: one shared fixture.
  const fixture = read("tests/fixtures/navigation-policy.tsv");
  assert.match(rs, /include_str!\("\.\.\/\.\.\/\.\.\/tests\/fixtures\/navigation-policy\.tsv"\)/);
  const ktTest = read(`${KOTLIN}/SiteNavigationPolicyTest.kt`);
  assert.match(ktTest, /const val FIXTURE = "navigation-policy\.tsv"/);
  // Rounds 22-23: the CONNECT request/authority grammar is shared the same way,
  // so a method, version, character, bracket or port rule added to one proxy
  // parser alone is refused by the other suite.
  const connectFixture = read("tests/fixtures/connect-request.tsv");
  const PROXY_RS = read("src-tauri/src/web/pinned_proxy.rs");
  const PROXY_KT_TEST = read(`${KOTLIN}/DnsPinnedProxyTest.kt`);
  assert.match(
    PROXY_RS,
    /include_str!\("\.\.\/\.\.\/\.\.\/tests\/fixtures\/connect-request\.tsv"\)/,
  );
  assert.match(PROXY_KT_TEST, /const val CONNECT_FIXTURE = "connect-request\.tsv"/);
  assert.match(PROXY_KT_TEST, /getResourceAsStream\(CONNECT_FIXTURE\)/);
  // Both suites must check BOTH row kinds: an authority-only fixture cannot
  // catch a method or version divergence.
  for (const src of [PROXY_RS, PROXY_KT_TEST]) {
    // Rust uses `=>`, Kotlin `->`; both must branch on both row kinds.
    assert.match(src, /"request"\s*(=>|->)/, "the request row kind must be consumed");
    assert.match(src, /"authority"\s*(=>|->)/, "the authority row kind must be consumed");
  }
  const connectVectors = connectFixture
    .split("\n")
    .filter((line) => line.includes("\t") && !line.startsWith("#"));
  const byKind = (kind) => connectVectors.filter((line) => line.startsWith(`${kind}\t`));
  assert.ok(
    byKind("request").length >= 30,
    `request vectors: ${byKind("request").length} is too few to pin the line rules`,
  );
  assert.ok(
    byKind("authority").length >= 30,
    `authority vectors: ${byKind("authority").length} is too few to pin the authority rules`,
  );
  // The forms that used to diverge between the platforms, and the ones that must
  // keep working, have to stay in the fixture.
  const connectText = connectVectors.join("\n");
  for (const needle of [
    // Round 22: desktop-only acceptances.
    "example.com:+443\treject",
    "[8.8.8.8]:443\treject",
    "example.com:443:443\treject",
    "[::1]:443\taccept",
    // Round 23: method case, version, and the authorities that used to reach
    // the desktop resolver.
    "connect example.com:443 HTTP/1.1\treject",
    "Connect example.com:443 HTTP/1.1\treject",
    "CONNECT example.com:443 HTTP/1.2\treject",
    "CONNECT example.com:443 HTTP/1.foo\treject",
    "authority\tfoo[bar:443\treject",
    "authority\tfoo]bar:443\treject",
    "authority\tfoo..bar:443\treject",
    "authority\t[::::]:443\treject",
    "authority\t[1:2:3:4:5:6:7:8:9]:443\treject",
    // The port policy that must NOT be confused: five digits is fine, six is not.
    "authority\th.example:070\taccept",
    "authority\texample.com:0000443\treject",
  ]) {
    assert.ok(connectText.includes(needle), `missing connect vector: ${needle}`);
  }
  const kinds = ["url", "host", "ip"];
  for (const kind of kinds) {
    const count = fixture
      .split("\n")
      .filter((line) => line.startsWith(`${kind}\t`) && !line.startsWith("#")).length;
    assert.ok(count >= 30, `${kind} vectors: ${count} is too few to pin parity`);
  }
  // And the interesting cases really are in there.
  for (const needle of [
    "data:text/html",
    "about:blank",
    "about:srcdoc",
    "blob:https://openani.me",
    "javascript:alert(1)",
    "https://0x7f000001/",
    "https://tauri.localhost/",
    "example.0x7f",
    "0177.0.0.1",
    "169.254.169.254",
    "2001:db8::1",
    "::ffff:8.8.8.8",
  ]) {
    assert.ok(fixture.includes(needle), `fixture lost the ${needle} vector`);
  }
});

test("navigation policy wraps the site client too", () => {
  // Same guard as the launcher: scheme/host policy stays mandatory. The
  // guard was split out of AnimeHubPlugin.kt so the CI unit test can
  // type-check it — android_prepare.py MUST copy the new file or neither
  // the APK nor the test compile resolves it.
  // The guard is built once into a field so every teardown path can shut its
  // resolver thread down (`SiteNavigationGuard.close`); it is still the only
  // client the site WebView ever gets.
  assert.match(controllerKt, /val guard = SiteNavigationGuard\(SiteWebViewClient\(/);
  assert.match(controllerKt, /webView\.webViewClient = guard\n/);
  assert.match(controllerKt, /navigationGuard = guard/);
  assert.match(controllerKt, /private fun releaseNavigationGuard\(\)/);
  assert.match(controllerKt, /guard\.close\(\)/);
  assert.doesNotMatch(
    controllerKt,
    /webView\.webViewClient = SiteWebViewClient\(/,
    "the raw client must never be installed without the policy guard",
  );
  const guardKt = read(
    "src-tauri/android-plugin/kotlin/dev/animehub/app/SiteNavigationGuard.kt",
  );
  assert.match(guardKt, /class SiteNavigationGuard/);
  assert.match(guardKt, /InetAddress\.getAllByName\(host\)/);
  // The verdict cache is shared by both checkpoints (round 6): the guard keeps
  // the resolver calls, NavigationDnsGate owns classify/caching/bounds.
  assert.match(guardKt, /dnsGate\.decide\(uri\.host/);
  assert.match(guardKt, /dnsGate\.cachedVerdict\(host, now\)/);
  const dnsGateKt = read(
    "src-tauri/android-plugin/kotlin/dev/animehub/app/NavigationDnsGate.kt",
  );
  assert.match(dnsGateKt, /ResolvedHostPolicy\.classify\(previous, answer\)/);
  assert.match(dnsGateKt, /DnsVerdict\.PRIVATE_ADDRESS/);
  assert.match(dnsGateKt, /MAX_HOSTS/);
  assert.match(preparePy, /"NavigationDnsGate"/);
  assert.match(read(`${KOTLIN}/SiteNavigationPolicy.kt`), /object ResolvedHostPolicy/);
  assert.match(read(`${KOTLIN}/SiteNavigationPolicy.kt`), /CHANGED_ADDRESS/);
  assert.match(guardKt, /SiteNavigationPolicy\.rejection\(uri\)/);
  assert.match(preparePy, /"SiteNavigationGuard"/);
  assert.match(preparePy, /"SiteBlocklist"/);
  assert.match(preparePy, /"AnimeHubDebugLog"/);
  assert.match(preparePy, /"SiteProfileOps"/);
});

test("connection pinning is wired from the proxy to the refusal", () => {
  // Round 16: the DNS gate is a pre-check; the GUARANTEE is that every site
  // request leaves through the loopback proxy, which resolves a hostname once
  // and connects to that exact address. These assertions pin the wiring that
  // makes that true, and the absence of every direct path around it.
  const proxyKt = read(`${KOTLIN}/DnsPinnedProxy.kt`);
  const connectKt = read(`${KOTLIN}/PinnedConnect.kt`);
  const gateKt = read(`${KOTLIN}/SiteProxyGate.kt`);
  const adapterKt = read(`${KOTLIN}/SiteProxyController.kt`);
  // Code only: the headers NAME the unsafe form to explain why it is absent.
  const codeOf = (src) => src
    .split("\n")
    .filter((line) => {
      const t = line.trim();
      return t !== "" && !t.startsWith("//") && !t.startsWith("*") && !t.startsWith("/*");
    })
    .join("\n");
  const connectCode = codeOf(connectKt);
  const proxyCode = codeOf(proxyKt);
  const adapterCode = codeOf(adapterKt);

  // Loopback only, ephemeral port, no wildcard listener anywhere.
  assert.match(proxyKt, /const val LOOPBACK: String = "127\.0\.0\.1"/);
  assert.doesNotMatch(proxyKt, /"0\.0\.0\.0"/);
  assert.match(proxyKt, /socket\.bind\(InetSocketAddress\(InetAddress\.getByName\(LOOPBACK\), 0\)/);

  // CONNECT is the only method; anything else (cleartext HTTP) is refused.
  assert.match(proxyKt, /if \(parts\[0\] != "CONNECT"\) return null/);
  assert.match(proxyKt, /Refusal\.METHOD -> METHOD_NOT_ALLOWED/);

  // One resolution per connection, then classification of EVERY address: a
  // mixed answer is refused as a whole, never "the public half".
  assert.match(proxyKt, /resolutions\.incrementAndGet\(\)/);
  assert.match(proxyKt, /if \(!IpPolicy\.isPublicIp\(text\)\)/);
  assert.match(proxyKt, /Refusal\.NON_PUBLIC/);

  // The socket is opened from the CLASSIFIED address (never a name) and the
  // result is verified against it.
  assert.match(connectKt, /InetSocketAddress\(address, port\)/);
  assert.doesNotMatch(connectCode, /Socket\(\s*(host|hostname|name)/, "a name-based socket would resolve again");
  assert.doesNotMatch(proxyCode, /Socket\(\s*target\.host/);
  assert.match(connectKt, /actual\.address\.contentEquals\(expected\.address\)/);
  assert.match(proxyKt, /if \(!sameAddress\(socket, address\)\)/);

  // Bounded resources: header cap, buffer, idle/connect timeouts, ceiling.
  assert.match(proxyKt, /headerBytes: Int = 8 \* 1024/);
  assert.match(proxyKt, /maxConnections: Int = 16/);
  assert.match(proxyKt, /SynchronousQueue/);
  assert.match(proxyKt, /Refusal\.BUSY/);

  // Round 16.2: the loopback bind is an EGRESS control, never app isolation.
  // The old KDoc claimed the port could not be reached "from another app on
  // the device", which the platform facts contradict (Android loopback is
  // shared), so the wording is pinned as well as the behaviour.
  assert.match(proxyKt, /not an isolation boundary/);
  assert.doesNotMatch(
    proxyKt,
    /must not be[\s\S]{0,40}reachable from the network, from another app/,
    "the listener is reachable from other apps on the device; do not claim isolation",
  );

  // Resolver: a bounded pool per generation plus a hard ceiling. A stuck
  // lookup cannot be cancelled, so the cap — not the pool size — is what keeps
  // threads bounded, and past the cap the refusal is its own reason.
  assert.match(proxyKt, /resolveThreads: Int = 4/);
  assert.match(proxyKt, /resolveThreadCap: Int = 8/);
  assert.match(proxyKt, /Refusal\.RESOLVE_EXHAUSTED/);
  assert.match(proxyKt, /newResolverGeneration\(\)/);
  assert.match(proxyKt, /resolveGenerations/);
  // The cap must leave room for a whole spare generation: 4/5 used to be
  // accepted and then could never form one.
  assert.match(proxyKt, /require\(resolveThreadCap >= 2 \* resolveThreads\)/);
  // Round 16.3: an older generation with a free thread is reused instead of
  // refusing because the cap is already spent, and `stop()` must NOT forget the
  // generations whose calls are still running (that is how a restart spent the
  // same budget twice).
  assert.match(proxyKt, /private fun generationOrder\(current: ThreadPoolExecutor\)/);
  assert.match(proxyKt, /for \(generation in generationOrder\(current\)\)/);
  assert.match(proxyKt, /private fun liveResolverThreads\(\)/);
  assert.doesNotMatch(
    proxyCode,
    /resolveGenerations\.clear\(\)/,
    "stop() must keep tracking generations: their threads still exist",
  );
  // The budget is enforced before the listener exists, so a start without a
  // resolver thread cannot bind and cannot look ready.
  assert.match(proxyKt, /val generation = try \{\s*\n\s*newResolverGeneration\(\)/);
  // stop() must count what a shut-down generation is still running, and must
  // not snapshot the live-socket set with `toList()`: that iteration can throw
  // NoSuchElementException when a handler removes its own socket mid-sweep, and
  // a stop that throws there leaves descriptors open.
  assert.match(proxyKt, /if \(generation.isShutdown\) generation.activeCount/);
  assert.doesNotMatch(
    proxyCode,
    /live\.toList\(\)/,
    "stop() must sweep the socket set without a toList() snapshot",
  );
  assert.match(proxyKt, /catch \(_: java\.util\.NoSuchElementException\)/);
  // The tests must model an UNCANCELLABLE lookup: an interruptible fake made
  // the round-16.1 "stuck" suite pass while proving the opposite case.
  const proxyTestKt = read(`${KOTLIN}/DnsPinnedProxyTest.kt`);
  assert.match(proxyTestKt, /class UninterruptibleResolver/);
  assert.match(proxyTestKt, /fun an_older_generation_is_reused_once_its_lookup_returns\(\)/);
  assert.match(proxyTestKt, /fun a_restart_cannot_spend_the_resolver_budget_twice\(\)/);
  assert.match(proxyTestKt, /fun the_cap_must_leave_room_for_a_full_spare_generation\(\)/);

  // Round 16.4: start() is all-or-nothing. The validation runs before any
  // socket exists, the phases are injectable so each one is covered by a real
  // rollback test, and the rollback must put the published fields back —
  // otherwise a failed start leaves `port` set and the controller would install
  // a proxy that is not accepting.
  assert.match(proxyKt, /internal fun validate\(\)/);
  assert.match(proxyKt, /limits\.validate\(\)\n\n    \/\/ 2\./);
  assert.match(proxyKt, /require\(maxConnections > 0\)/);
  assert.match(proxyKt, /require\(headerBytes in 1\.\.MAX_HEADER_BYTES\)/);
  assert.match(proxyKt, /require\(bufferBytes in 1\.\.MAX_BUFFER_BYTES\)/);
  assert.match(proxyKt, /require\(resolveThreads <= Int\.MAX_VALUE \/ 2\)/);
  assert.match(proxyKt, /fun interface StartHook/);
  assert.match(proxyKt, /enum class StartPhase/);
  assert.match(proxyKt, /releaseUnusedGeneration\(generation\)/);
  // Every published field is reset in the rollback.
  for (const [field, value] of [
    ["running", "false"],
    ["server", "null"],
    ["handlerPool", "null"],
    ["pumpPool", "null"],
    ["resolvePool", "null"],
    ["acceptThread", "null"],
    ["boundPort", "null"],
  ]) {
    assert.match(
      proxyKt,
      new RegExp(`\\n[ \\t]+${field} = ${value}[ \\t]*\\n`),
      `${field} must be reset when start() rolls back`,
    );
  }
  assert.match(proxyKt, /internal fun resolverGenerationCount\(\)/);
  assert.match(proxyTestKt, /fun an_invalid_limits_value_cannot_even_be_constructed\(\)/);
  assert.match(proxyTestKt, /fun a_failed_start_leaves_nothing_behind_and_can_be_retried\(\)/);
  assert.match(proxyTestKt, /fun a_failed_start_returns_its_resolver_generation_to_the_budget\(\)/);
  // A failed start is a refusal, never a ready proxy: the controller stops the
  // half-started instance and marks the gate failed, and only the install
  // callback may mark it ready.
  assert.match(
    adapterKt,
    /catch \(e: Exception\) \{[\s\S]{0,900}?stopQuietly\(\)\n      gate\.markFailed\(\)\n      return\n    \}/,
  );
  assert.equal(
    adapterKt.match(/gate\.markReady\(\)/g).length,
    1,
    "only the setProxyOverride callback may mark the gate ready",
  );

  // The ONLY configuration: one loopback rule, implicit bypass removed, and no
  // bypass/DIRECT/clear call that would restore a direct path.
  assert.match(gateKt, /const val LOOPBACK_HOST: String = "127\.0\.0\.1"/);
  assert.match(adapterCode, /\.addProxyRule\(rule\)/);
  assert.match(adapterCode, /\.removeImplicitRules\(\)/);
  for (const forbidden of ["addBypassRule", "addDirect", "bypassSimpleHostnames", "clearProxyOverride"]) {
    assert.ok(!adapterCode.includes(forbidden), `${forbidden} is a direct path and must never be used`);
  }
  assert.equal(adapterCode.match(/addProxyRule\(/g).length, 1, "one rule, never a fallback list");
  assert.match(adapterKt, /WebViewFeature\.isFeatureSupported\(WebViewFeature\.PROXY_OVERRIDE\)/);

  // The open path refuses when the pin is not confirmed, before any view or
  // navigation exists, and Rust maps that refusal to user-facing guidance.
  assert.match(controllerKt, /SiteProxyController\.ensureReady\(operationId\)/);
  assert.match(controllerKt, /onResult\(SiteProxyController\.ERR_PROXY_UNAVAILABLE\)/);
  assert.match(pluginLibRs, /pub const PROXY_SENTINEL: &str = "proxy-unavailable"/);
  assert.match(bridgeRs, /animehub_android::is_proxy_unavailable\(&error\)/);
  const openIndex = controllerKt.indexOf("SiteProxyController.ensureReady(operationId)");
  const loadIndex = controllerKt.indexOf("webView.loadUrl(url)");
  assert.ok(openIndex > 0 && loadIndex > openIndex, "the pin is confirmed before the first navigation");

  // Packaging and CI copy: a dropped file must not silently remove the tests.
  for (const f of ["DnsPinnedProxy", "PinnedConnect", "SiteProxyGate", "SiteProxyController"]) {
    assert.ok(preparePy.includes(`"${f}"`), `android_prepare.py must copy ${f}`);
  }
  const ci = read(".github/workflows/ci.yml");
  for (const f of ["DnsPinnedProxyTest.kt", "SiteProxyGateTest.kt"]) {
    assert.ok(ci.includes(`app/${f}`), `CI must copy ${f} into the test source set`);
  }
});

// ---------------------------------------------------------------------------
// Build pipeline
// ---------------------------------------------------------------------------

test("the pure Kotlin split is packaged, pure, copied and homed", () => {
  for (const f of PURE_FILES) {
    const src = pureFile(f);
    assert.match(src, /^\/\/ SPDX-License-Identifier: MIT/, `${f} lost its header`);
    assert.match(src, /^package dev\.animehub\.app$/m, `${f} lost its package line`);
    assert.doesNotMatch(src, /^import android\./m, `${f} must stay pure (no android.*)`);
    assert.doesNotMatch(src, /^import app\.tauri/m, `${f} must stay pure (no Tauri)`);
    // android_prepare.py hard-fails when one of them is missing, so the CI
    // unit-test compile cannot silently lose a responsibility.
    assert.ok(
      preparePy.includes(`"${f.replace(/\.kt$/, "")}"`),
      `android_prepare.py must copy ${f} into the generated project`,
    );
  }
  const homes = {
    "SiteProfileOps.kt": [
      "object SiteProfileErrors",
      "class SiteSessionState",
      "object GeolocationWipe",
      "sealed interface Watchdog",
      "object WatchdogArming",
    ],
    "ProfileQuarantine.kt": ["object ProfileQuarantine", "object SiteOpenGate", "object ProfileRetirement"],
    "SiteCloseFlow.kt": ["interface SiteClosePort", "object SiteCloseFlow"],
    "SiteShutdownFlow.kt": ["object ShutdownFlow", "sealed interface ShutdownFlush"],
    "SiteClearFlow.kt": ["object SiteClearFlow", "object CookieWipes", "interface SiteClearPort"],
    "SiteMigrationFlow.kt": ["object SiteMigrationFlow", "sealed class MigrationOutcome"],
    "SiteBackStatus.kt": ["object SiteBackStatus"],
    "DnsPinnedProxy.kt": ["class DnsPinnedProxy", "fun parseConnectHead", "data class Limits"],
    "PinnedConnect.kt": ["object PinnedConnect", "class AddressChangedException"],
    "SiteProxyGate.kt": ["object SiteProxyPolicy", "class SiteProxyGate", "enum class State"],
  };
  for (const [f, markers] of Object.entries(homes)) {
    for (const m of markers) {
      assert.ok(pureFile(f).includes(m), `${f} must be the home of ${m}`);
    }
  }
  // The worker file is part of the same split: copied into the generated
  // project, packaged, and the ONLY home of the executor.
  assert.match(flushWorkerKt, /^package dev\.animehub\.app$/m);
  assert.ok(
    preparePy.includes('"SiteFlushWorker"'),
    "android_prepare.py must copy SiteFlushWorker.kt into the generated project",
  );
  assert.equal((flushWorkerKt.match(/object SiteFlushWorker/g) || []).length, 1);

  // The split must not duplicate a rule: each key contract has exactly one home.
  for (const marker of [
    "object ProfileQuarantine",
    "object SiteOpenGate",
    "object SiteClearFlow",
    "object CookieWipes",
    "object ShutdownFlow",
    "object SiteMigrationFlow",
    "object GeolocationWipe",
    "val SITE_PROFILE_NAME",
  ]) {
    const count = PURE_FILES.filter((f) => pureFile(f).includes(marker)).length;
    assert.equal(count, 1, `${marker} must live in exactly one file`);
  }
});

test("android_prepare copies the site controller and pins androidx.webkit", () => {
  assert.match(preparePy, /SITE_CONTROLLER_CLASS = "AnimeHubSiteController"/);
  assert.match(preparePy, /raise SystemExit\(f"site controller not found at \{site_controller_src\}"\)/);
  assert.match(preparePy, /copied \{SITE_CONTROLLER_CLASS\}\.kt/);
  // Reproducible pin, replacing whatever the generated template shipped.
  assert.match(preparePy, /WEBKIT_PIN = 'implementation\("androidx\.webkit:webkit:1\.16\.0"\)'/);
  assert.match(preparePy, /WEBKIT_DEP_RE = re\.compile\(/);
  assert.match(preparePy, /pinned androidx\.webkit:webkit:1\.16\.0/);
  // The pin must be a stable release with no RC/alpha sibling referenced.
  assert.equal(/webkit:1\.\d+\.\d+-(alpha|beta|rc)/.test(preparePy), false);
  // Version compatibility, not error suppression: 1.16.0 resolves
  // kotlin-stdlib 2.1.20, so the generated project's compiler must join the
  // same family (KGP 2.1.20). Both halves of the alignment are pinned in the
  // prepare script; the CI unit-test run compiles with this same project.
  assert.match(
    preparePy,
    /KGP_PIN = 'classpath\("org\.jetbrains\.kotlin:kotlin-gradle-plugin:2\.1\.20"\)'/,
  );
  assert.match(preparePy, /KGP_DEP_RE = re\.compile\(/);
  assert.match(preparePy, /pinned kotlin-gradle-plugin:2\.1\.20/);
  assert.match(preparePy, /def verify_pin_state\(root_gradle: Path, app_gradle: Path\)/);
  assert.match(preparePy, /verify_pin_state\(root_gradle, gradle\)/);
  assert.match(preparePy, /androidx\.webkit pin is not exactly one occurrence/);
  const ciYml = read("ci-fixes/ci.yml");
  assert.match(ciYml, /Verify resolved Android pins/);
  assert.match(ciYml, /:app:dependencies --configuration debugRuntimeClasspath/);
  assert.match(ciYml, /buildEnvironment/);
  assert.match(preparePy, /kotlin-stdlib:2\.1\.20/, "the alignment rationale stays documented");
});

test("the destroy-time site flush is awaited within a bounded budget", () => {
  const ops = pureKotlin;
  // The protocol and its four honest outcomes.
  assert.match(ops, /sealed interface ShutdownFlush/);
  for (const outcome of ["NotNeeded", "Completed", "TimedOut"]) {
    assert.match(ops, new RegExp(`object ${outcome} : ShutdownFlush`));
  }
  assert.match(ops, /data class Failed\(val reason: String\) : ShutdownFlush/);
  assert.match(ops, /const val DESTROY_FLUSH_BUDGET_MS: Long = 1_000/);
  assert.match(ops, /require\(budgetMillis > 0\)/);
  const flowAt = ops.indexOf("object ShutdownFlow");
  const flow = ops.slice(flowAt, ops.indexOf("\n}\n", flowAt));
  assert.ok(
    flow.indexOf("port.submitFlush()") < flow.indexOf("port.teardownView()"),
    "the flush is handed to the worker before teardown, never run inline",
  );
  assert.ok(
    flow.indexOf("port.teardownView()") < flow.indexOf("port.awaitFlush(budgetMillis)"),
    "the bounded wait follows teardown, so the two overlap",
  );
  assert.match(flow, /if \(!port\.awaitFlush\(budgetMillis\)\) \{\s*ShutdownFlush\.TimedOut\s*\} else \{/);

  // The controller side: worker-submitted I/O, per-call completion state,
  // bounded await, teardown for every outcome, honest reporting.
  const destroyAt = controllerKt.indexOf("fun destroy(activity: Activity, operationId: String");
  assert.ok(destroyAt > 0);
  const destroy = controllerKt.slice(
    destroyAt,
    controllerKt.indexOf("private fun ensureBackCallback", destroyAt),
  );
  assert.match(destroy, /ShutdownFlow\.run\(/);
  assert.match(destroy, /val finished = CountDownLatch\(1\)/);
  assert.match(destroy, /val failure = AtomicReference<String\?>\(null\)/);
  assert.match(destroy, /SiteFlushWorker\.execute\("destroy", operationId\) \{/);
  assert.match(destroy, /finished\.await\(budgetMillis, TimeUnit\.MILLISECONDS\)/);
  assert.match(destroy, /finished\.countDown\(\)/);
  assert.doesNotMatch(
    code(destroy),
    /flush scheduled, not awaited/,
    "the unawaited-flush teardown must not come back",
  );
  const submitAt = destroy.indexOf("override fun submitFlush()");
  const submitBody = destroy.slice(submitAt, destroy.indexOf("override fun teardownView()"));
  assert.match(submitBody, /target\.cookieManager\.flush\(\)/);
  assert.doesNotMatch(
    code(submitBody),
    /\bactivity\b/,
    "the flush task must not capture the Activity (no leak past teardown)",
  );
  assert.match(destroy, /override fun teardownView\(\)/);
  assert.match(destroy, /webView\.destroy\(\)/);
  assert.match(destroy, /outcome == ShutdownFlush\.TimedOut/);
  assert.match(destroy, /durability NOT guaranteed/);

  // The generated Activity awaits destroy() before killing the process, and no
  // longer presents the default-profile flush as site-profile evidence.
  const generatedAt = preparePy.indexOf("override fun onDestroy()");
  const generated = preparePy.slice(generatedAt, preparePy.indexOf("killProcess", generatedAt));
  assert.ok(
    generated.indexOf("AnimeHubSiteController.destroy(this)") <
      generated.indexOf("CookieManager.getInstance().flush()"),
    "the site profile's awaited flush precedes the default-profile flush",
  );
  assert.ok(
    generated.indexOf("CookieManager.getInstance().flush()") <
      preparePy.indexOf("killProcess", generatedAt) - generatedAt,
    "the process is killed only after both flushes",
  );
  assert.match(generated, /NOT evidence that the site profile's store landed/);
  assert.doesNotMatch(
    generated,
    /blocks until the cookies are written, so the kill comes after them/,
    "the old false durability claim must be gone",
  );
});

test("the injected lifecycle block owns the site view across pause/resume/destroy", () => {
  assert.match(preparePy, /override fun onResume\(\)/);
  assert.match(preparePy, /AnimeHubSiteController\.onPause\(this\)/);
  assert.match(preparePy, /AnimeHubSiteController\.onResume\(this\)/);
  assert.match(preparePy, /AnimeHubSiteController\.destroy\(this\)/);
  // Idempotency markers stay (refresh replaces, never appends).
  assert.match(preparePy, /PIP_BLOCK_BEGIN/);
  assert.match(preparePy, /PIP_BLOCK_END/);
});

// ---------------------------------------------------------------------------
// Documentation honesty
// ---------------------------------------------------------------------------

test("docs describe the profile architecture, migration and what survives", () => {
  const doc = read("docs/android-storage-isolation.md");
  assert.match(doc, /Profile/i);
  assert.match(doc, /MULTI_PROFILE/);
  assert.match(doc, /1\.16\.0/);
  assert.match(doc, /kotlin-gradle-plugin:2\.1\.20/);
  // Migration honesty: cookies migrate, snapshots do not.
  assert.match(doc, /cookie/i);
  assert.match(doc, /localStorage/i);
  assert.match(doc, /not re-imported|NOT re-injected|never re-injected/i);
  // Verification honesty: no device claim.
  assert.match(doc, /cihazda test edilmedi|not device-tested|device-test/i);
  const changelog = read("CHANGELOG.md");
  assert.match(changelog, /Unreleased/i);
  assert.match(changelog, /profil/i);
});



test("the pinning proxy is documented as design-verified, not device-verified", () => {
  // Round 16 closed the TOCTOU gap by construction (the proxy owns the
  // connect), but nothing has run on a device. The docs are therefore required
  // to say three things at once — and this test fails if the balance slips:
  // (a) R-01 (b) is NOT reported fixed, (b) the proxy is named as the
  // guarantee while NavigationDnsGate is demoted to defence in depth, and
  // (c) a device checklist exists and states it has not been run.
  const risks = read("docs/security-residual-risks.md");
  const audit = read("docs/audit-item-status.md");
  const checklist = read("docs/android-dns-pinning-device-checklist.md");

  // (a) never "fixed" for the DNS half. The R-01 row may only say OPEN /
  // mitigated-by-design until the checklist passes.
  // The row spans several physical lines (sub-items (a)/(b)/(c)), so collect
  // the block up to the next table row before reading it.
  const lines = audit.split("\n");
  const from = lines.findIndex((line) => line.startsWith("| **R-01**"));
  assert.ok(from >= 0, "the R-01 row must exist");
  const to = lines.findIndex((line, i) => i > from && line.startsWith("| **R-02**"));
  const row = lines.slice(from, to > from ? to : from + 1).join("\n");
  assert.match(row, /OPEN/i);
  assert.equal(
    /\*\*\(b\) DNS[^*]*\*\*fixed/i.test(row),
    false,
    "R-01 (b) cannot be marked fixed before a device run",
  );
  assert.match(row, /android-dns-pinning-device-checklist/);

  // (b) which component is the guarantee, stated rather than implied.
  assert.match(risks, /Round 16 — DNS rebinding is decided where the connection is made/);
  assert.match(risks, /defence in depth/);
  assert.match(risks, /NavigationDnsGate/);
  assert.match(risks, /direct fallback/i);
  assert.match(risks, /device verification|No device verification yet/i);
  const storage = read("docs/android-storage-isolation.md");
  assert.match(storage, /refused at CONNECT by the pinning proxy/);
  assert.equal(
    /no DNS at all/.test(storage),
    false,
    "the old 'Android does no DNS' claim must not survive the proxy",
  );

  // (c) the checklist is a real script, and admits that it is unrun.
  assert.match(checklist, /NOT YET RUN/);
  for (const item of [
    /PROXY_OVERRIDE/,
    /rebind/i,
    /tcpdump/,
    /launcher/i,
    /IPC/,
    /WSS|WebSocket/,
    /QUIC/,
    /PiP/,
    /MULTI_PROFILE|cookie/i,
    /process.*(kill|recreation)|force-stop/i,
  ]) {
    assert.match(checklist, item);
  }
});

test("the user blocklist is pushed from Rust into the Android WebView", () => {
  const bridgeRs = read("src-tauri/src/android_bridge.rs");
  const pluginLib = read("src-tauri/android-plugin/src/lib.rs");
  const pluginKt = read(`${KOTLIN}/AnimeHubPlugin.kt`);
  const commandsRs = read("src-tauri/src/commands.rs");
  const policyKt = read(`${KOTLIN}/SiteNavigationPolicy.kt`);

  // Transport: Rust command, Kotlin command, and a call before every open.
  assert.match(pluginLib, /CMD_SITE_BLOCKLIST_UPDATE: &str = "site_blocklist_update"/);
  assert.match(pluginLib, /pub fn site_blocklist_update\(/);
  assert.match(bridgeRs, /pub fn site_blocklist_update\(entries: &\[String\]\)/);
  assert.match(pluginKt, /fun site_blocklist_update\(invoke: Invoke\)/);
  assert.match(pluginKt, /SiteBlocklist\.update\(rules\)/);
  assert.match(commandsRs, /fn sync_android_blocklist\(app: &AppHandle, state: &AppState\)/);
  assert.match(
    commandsRs,
    /fn push_android_blocklist\(entries: &\[String\]\)[\s\S]{0,220}site_blocklist_update\(entries\)/,
  );
  const syncCalls = commandsRs.match(/sync_android_blocklist\(&app, &state\)/g) || [];
  assert.ok(syncCalls.length >= 3, "open/settings/reset must all transfer the snapshot");
  assert.match(
    commandsRs,
    /pub fn reset_blocklist\(app: AppHandle, state: State<'_, AppState>\)[\s\S]{0,900}sync_android_blocklist\(&app, &state\)\?/,
  );
  assert.match(commandsRs, /fn transfer_blocklist_snapshot/);
  assert.match(commandsRs, /Err\(push_error\) => match close\(\)/);
  assert.match(commandsRs, /close_active_android_site\(app\)/);
  assert.match(commandsRs, /blocklist_transfer_closes_the_live_site_before_returning_push_error/);

  // Matching: same three rule forms + lookalike safety, and enforcement in
  // BOTH the navigation and sub-resource checkpoints via hostAllowed.
  assert.match(blocklistKt, /data class BlocklistRules/);
  assert.match(blocklistKt, /fun isBlocked\(rawHost: String\?\): Boolean/);
  assert.match(blocklistKt, /fun parse\(raw: List<String>\): BlocklistRules/);
  assert.match(blocklistKt, /wildcardOnly/);
  assert.match(blocklistKt, /notdoubleclick\.net|lookalike/i);
  assert.match(policyKt, /SiteBlocklist\.isBlocked\(host\)/);
  assert.match(read("src-tauri/src/sites/blocklist.rs"), /pub fn entries\(&self\) -> Vec<String>/);
});
