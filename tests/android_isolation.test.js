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
];
const pureKotlin = PURE_FILES.map((f) => read(`${KOTLIN}/${f}`)).join("\n");
const pureFile = (f) => read(`${KOTLIN}/${f}`);
/**
 * The platform boundary: the single blocking-flush worker and the watchdog
 * arming. Not pure (it touches Handler/Looper/Activity) — which is exactly why
 * it lives outside the pure set.
 */
const flushWorkerKt = read(`${KOTLIN}/SiteFlushWorker.kt`);

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
    const signatureEnd = controllerKt.indexOf(") {", at);
    const guard = controllerKt.slice(signatureEnd, signatureEnd + 400);
    assert.match(
      guard,
      /if \(!isSupported\(\)\) \{\n\s*onResult\(ERR_UNSUPPORTED_MULTI_PROFILE\)/,
      `${entry} must refuse an unsupported provider first`,
    );
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
  assert.match(bridgeRs, /is_unsupported_multi_profile\(&e\)/);
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
  const settings = controllerKt.indexOf("webView.settings.javaScriptEnabled", buildAt);
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
  const protoAt = windowsRs.indexOf("pub fn run_mobile_open_protocol");
  assert.ok(protoAt > 0, "the two-phase protocol function is missing");
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
  assert.match(body, /kalıcılaştırılamadı/, "the abort must be user-visible");
  assert.doesNotMatch(
    body,
    /log::warn!\("profil göç bayrağı yazılamadı/,
    "a failed flag write must not be a mere warning any more",
  );
  // `open_on_mobile` delegates and claims the session only afterwards.
  const openAt = windowsRs.indexOf("fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {");
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
  assert.match(controllerKt, /onResult\("\$ERR_MIGRATION_RETRY: profil zaten mevcut"\)/);
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
  // windows.rs: delete → (queue) → bump → retire, all on the derived name.
  const at = windowsRs.indexOf("pub fn run_mobile_delete_protocol");
  assert.ok(at > 0, "the delete protocol is missing");
  const body = windowsRs.slice(at, windowsRs.indexOf("\n}\n", at));
  const del = body.indexOf("delete_profile(&profile)?");
  const queue = body.indexOf("queue_pending_profile_delete(state, &profile)?");
  const bump = body.indexOf("bump_profile_generation(state, site_id)?");
  const retire = body.indexOf("retire_profile(&profile)?");
  assert.ok(del > 0, "the deletion runs first");
  assert.ok(queue > del, "a deferred deletion is queued before the bump");
  assert.ok(bump > queue, "the durable bump follows the queue write");
  assert.ok(retire > bump, "THE ORDER: retirement only after the bump is durable");
  // A Pending profile still exists on disk → its ledger survives until the
  // startup drain lands the deletion.
  assert.match(body, /if !matches!\(action, SiteProfileAction::Pending\) \{[\s\S]*retire_profile/);
  // Host tests that execute the order itself.
  for (const name of [
    "mobile_delete_protocol_retires_only_after_the_generation_is_durable",
    "mobile_delete_protocol_keeps_the_ledger_when_the_generation_write_fails",
    "mobile_delete_protocol_queues_a_deferred_deletion_and_keeps_its_ledger",
    "mobile_delete_protocol_is_safe_to_retry_after_a_failed_bump",
    "mobile_delete_protocol_retires_each_generation_exactly_once_on_repeat",
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
    /fun retireProfileQuarantine\(profileName: String, onResult: \(dropped: Int, error: String\?\) -> Unit\)/,
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
  assert.match(pluginKt, /AnimeHubSiteController\.retireProfileQuarantine\(profileName\)/);
  // The other two deletion paths keep the same rule: durable step first.
  const sweep = commandsRs.slice(commandsRs.indexOf('orphan sweep for "clear all"'));
  assert.match(sweep, /site_profile_retire\(&name\)/, "the orphan sweep retires what it deleted");
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
  assert.match(
    windowsRs,
    /clear_profile\(&profile\)\?;\s+state\.provider\.delete_site_session\(site_id\)\?;/,
    "wipe the profile first, then drop the encrypted blobs",
  );
  // Close-without-flush precedes the wipes for the active site.
  assert.ok(body.indexOf("close_site_window_for_clear_locked") < body.indexOf("site_clear_data"));
  // Orphan sweep (is_all) addresses only our namespace and never owned names.
  const sweepAt = body.indexOf("site_profile_names");
  assert.ok(sweepAt > 0, "android orphan sweep missing");
  assert.match(body.slice(sweepAt), /name\.starts_with\(windows::ANDROID_PROFILE_PREFIX\)/);
  assert.match(body.slice(sweepAt), /owned\.contains\(&name\)/);
  // No shared/default-jar clearing call anywhere in the command.
  assert.doesNotMatch(body, /webview_cookies_clear/);
});

test("deferred deletions are persisted, drained at startup, and deduped", () => {
  assert.match(windowsRs, /pub const PENDING_PROFILE_DELETES_KEY: &str = "android-pending-profile-deletes"/);
  assert.match(windowsRs, /pub fn queue_pending_profile_delete/);
  assert.match(windowsRs, /pub fn forget_pending_profile_delete/);
  // lib.rs drains before the frontend can open a site.
  const libRs = read("src-tauri/src/lib.rs");
  const manageAt = libRs.indexOf("app.manage(AppState {");
  const drainAt = libRs.indexOf("drain_pending_profile_deletes");
  const routeAt = libRs.indexOf("// Route the OAuth redirect");
  assert.ok(manageAt > 0 && drainAt > manageAt, "drain must start after the state is managed");
  assert.ok(drainAt < routeAt, "drain spawns during setup, not later");
  assert.match(libRs, /fn drain_pending_profile_deletes/);
  // A pending result is never reported as deleted.
  assert.match(libRs, /Ok\(SiteProfileAction::Pending\) => \{\s*\/\/ Still in use/);
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
  assert.match(pluginKt, /AnimeHubSiteController\.migrate\(activity, url, profileName, cookies\)/);
  assert.match(pluginKt, /AnimeHubSiteController\.open\(activity, url, profileName, initScript\)/);
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
    /if \(!settled\.compareAndSet\(false, true\)\) return@flushCookies/,
    "a late flush callback must lose the race",
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
    /if \(!flushSettled\.compareAndSet\(false, true\)\) return@flushCookies/,
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
    /SiteClearFlow\.run\(isSupported\(\), profileName, port\)/,
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
    (bridgeRs.match(/\.map_err\(classify_site_open_error\)/g) || []).length,
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
    /watchdog could not be armed \(postDelayed returned false\)/,
    "the pure decision must name the false-return contract",
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
    /no operation\s*\n?\s*\/\/ is left outstanding|nothing is quarantined|is deliberately not/,
    "the branch must say why nothing is quarantined",
  );
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
  // ONE executor, in ONE file: nothing in the controller may queue flush work
  // behind the worker's back (the destroy-time teardown goes through it too, so
  // a stuck provider flush is observable there as well).
  assert.equal((flushWorkerKt.match(/FLUSH_EXECUTOR\.execute/g) || []).length, 1);
  // Only documentation may still name it; no controller CODE may queue behind
  // the worker's back.
  assert.doesNotMatch(code(controllerKt), /FLUSH_EXECUTOR/);
  assert.equal(
    (controllerKt.match(/SiteFlushWorker\.execute\("destroy"\)/g) || []).length,
    1,
    "the destroy-time flush uses the same worker",
  );
  const destroyAt = controllerKt.indexOf("fun destroy(activity: Activity)");
  assert.ok(destroyAt > 0);
  const destroyBody = controllerKt.slice(
    destroyAt,
    controllerKt.indexOf("private fun ensureBackCallback", destroyAt),
  );
  assert.match(destroyBody, /SiteFlushWorker\.execute\("destroy"\)/);
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
  assert.match(flushWorkerKt, /flush worker stuck/);
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
  assert.match(controllerKt, /closeViaLauncher\(activity\)/);
  // The callback is disabled when no site is open, so the launcher's own
  // default back behaviour (Tauri: no wry callback) stays as before.
  assert.match(controllerKt, /private var backCallback: OnBackPressedCallback\?/);
  assert.match(controllerKt, /backCallback\?\.isEnabled = false/);
  // The launcher-side hook returns whether a close session ACCEPTED the
  // request (never a bare "it ran"), and exposes the status Kotlin polls
  // for the real outcome (pending | closed | failed:<reason> | idle).
  assert.match(
    mainJs,
    /window\.__animehubBackToLauncher = \(\) => backSession\.requestClose\(\)\.accepted;/,
  );
  assert.match(mainJs, /window\.__animehubBackStatus = \(\) => backSession\.statusString\(\);/);
  // A.5: the label must not be dropped in a finally after invoking close.
  assert.doesNotMatch(mainJs, /finally \{\s*\n\s*openSiteLabel = null;/);
  // No site-facing close-scheme ever appears on Android.
  assert.doesNotMatch(controllerKt, /animehub:\/\/close-site/);
});

test("PiP re-binds between the site WebView and the launcher", () => {
  // Open: bind to the site view. Close/failure/destroy: back to launcher.
  assert.match(controllerKt, /AnimeHubPipController\.attach\(activity, webView\)/);
  const launcherRebinds =
    controllerKt.match(/AnimeHubPipController\.attach\(activity, (it|launcher)\)/g) || [];
  assert.ok(
    launcherRebinds.length >= 2,
    "close and failure paths must re-bind to the launcher",
  );
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

test("navigation policy wraps the site client too", () => {
  // Same guard as the launcher: scheme/host policy stays mandatory. The
  // guard was split out of AnimeHubPlugin.kt so the CI unit test can
  // type-check it — android_prepare.py MUST copy the new file or neither
  // the APK nor the test compile resolves it.
  assert.match(controllerKt, /webView\.webViewClient = SiteNavigationGuard\(/);
  const guardKt = read(
    "src-tauri/android-plugin/kotlin/dev/animehub/app/SiteNavigationGuard.kt",
  );
  assert.match(guardKt, /class SiteNavigationGuard/);
  assert.match(guardKt, /SiteNavigationPolicy\.allows\(uri\)/);
  assert.match(preparePy, /"SiteNavigationGuard"/);
  assert.match(preparePy, /"AnimeHubDebugLog"/);
  assert.match(preparePy, /"SiteProfileOps"/);
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
  assert.match(flow, /if \(!port\.awaitFlush\(budgetMillis\)\) return ShutdownFlush\.TimedOut/);

  // The controller side: worker-submitted I/O, per-call completion state,
  // bounded await, teardown for every outcome, honest reporting.
  const destroyAt = controllerKt.indexOf("fun destroy(activity: Activity)");
  assert.ok(destroyAt > 0);
  const destroy = controllerKt.slice(
    destroyAt,
    controllerKt.indexOf("private fun ensureBackCallback", destroyAt),
  );
  assert.match(destroy, /ShutdownFlow\.run\(/);
  assert.match(destroy, /val finished = CountDownLatch\(1\)/);
  assert.match(destroy, /val failure = AtomicReference<String\?>\(null\)/);
  assert.match(destroy, /SiteFlushWorker\.execute\("destroy"\) \{/);
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
  assert.match(destroy, /ShutdownFlush\.TimedOut ->/);
  assert.match(destroy, /durability not guaranteed/);

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
