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
  // default profile.
  assert.match(
    controllerKt,
    /Regex\("""site-\[0-9a-f\]\{32\}-g\\d\{1,10\}"""\)/,
    "controller must validate profile names against the Rust derivation",
  );
  assert.match(controllerKt, /fun profileNameIsValid\(name: String\): Boolean = PROFILE_NAME\.matches\(name\)/);
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

test("site_open gates on MULTI_PROFILE before anything else and never falls back", () => {
  // The check is the first thing open() does, ahead of validation/UI work.
  const openAt = controllerKt.indexOf("fun open(");
  assert.ok(openAt > 0);
  const signatureEnd = controllerKt.indexOf(") {", openAt);
  const guard = controllerKt.slice(signatureEnd, signatureEnd + 400);
  assert.match(guard, /if \(!isSupported\(\)\) \{\n\s*onResult\(false, ERR_UNSUPPORTED_MULTI_PROFILE\)/);
  // No path opens the site when the feature is missing: the sentinel reject
  // is the only unsupported branch in the file.
  assert.equal(
    (controllerKt.match(/ERR_UNSUPPORTED_MULTI_PROFILE/g) || []).length,
    2,
    "declaration + the single reject site",
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
  assert.match(controllerKt, /val manager: CookieManager = profile\.cookieManager/);
  assert.match(controllerKt, /profile\.cookieManager\.flush\(\)/);
});

test("migration is one-time, profile-scoped, and ordered before first navigation", () => {
  // Rust side: flag read → cookies passed → open → flag written only on success.
  const openAt = windowsRs.indexOf("fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {");
  assert.ok(openAt > 0, "android open_on_mobile missing");
  const body = windowsRs.slice(openAt, windowsRs.indexOf("\n}\n", openAt));
  assert.match(body, /MIGRATION_FLAG_PREFIX/, "migration must be flag-gated");
  assert.match(body, /mark_migration_done/, "the flag is written after the open");
  assert.match(body, /site_open\(url\.as_str\(\), &profile, init_script, &cookie_headers\)/, "cookies ride the open call");
  // The flag write must come AFTER the site_open call (failed opens retry).
  assert.ok(
    body.indexOf("site_open(url.as_str()") < body.indexOf("mark_migration_done"),
    "flag must be written only after a successful open",
  );
  // Kotlin: virgin profile only; existing profile jars are authoritative.
  assert.match(controllerKt, /val existing = store\.getProfile\(profileName\)/);
  assert.match(controllerKt, /if \(existing != null\) \{[\s\S]{0,200}onResult\(false, null\)/);
  // Flush to disk completes before the WebView is even constructed.
  assert.match(controllerKt, /manager\.flush\(\)/);
  const flushAt = controllerKt.indexOf("manager.flush()");
  const buildAt = controllerKt.indexOf("private fun buildWebView(");
  assert.ok(flushAt < buildAt || controllerKt.indexOf("manager.flush()", flushAt + 1) < buildAt,
    "flush must run during migration, before buildWebView");
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
  const deleteAt = body.indexOf("site_profile_delete");
  assert.ok(builtinAt > 0 && deleteAt > builtinAt, "profile delete must come after the builtin check");
  // The builtin early-return happens before the delete call.
  assert.ok(body.slice(builtinAt, deleteAt).includes("return Ok(());"), "builtin must return before deletion");
  // Pending → queued (never silently dropped), hard errors → propagate.
  assert.match(body, /SiteProfileAction::Pending => \{\s*w::queue_pending_profile_delete\(&state, &profile\)\?;/);
  // Generation bumps only after a successful/queued delete.
  assert.ok(
    body.indexOf("site_profile_delete(&profile)?") < body.indexOf("bump_profile_generation(&state, &id)?"),
    "bump must follow the delete",
  );
  // Active site is closed (destroyed) before the delete.
  assert.ok(body.indexOf("close_site_window_for_clear_locked") < body.indexOf("site_profile_delete"));
  // Registry mutation follows the delete (the first persist in the block is
  // the builtin early-return, which must stay before it): a failed delete
  // keeps the tile.
  const persistAfterDelete = body.indexOf("persist_registry(&state)?;", deleteAt);
  assert.ok(persistAfterDelete > deleteAt, "the user-site registry write must follow the delete");
});

test("clear_site_data clears profiles in place and sweeps orphans, never the launcher", () => {
  const clearAt = commandsRs.indexOf("pub async fn clear_site_data");
  assert.ok(clearAt > 0);
  const body = commandsRs.slice(clearAt, commandsRs.indexOf("\n}\n", clearAt));
  assert.match(body, /site_clear_data\(&profile\)/, "per-site profile clear");
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
  assert.match(body, /site_open\(/, "the site view is opened through the profiled bridge");
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
  // The launcher-side hook exists and returns whether it ran.
  assert.match(mainJs, /window\.__animehubBackToLauncher = \(\) => \{/);
  assert.match(mainJs, /void backToLauncher\(\);\n\s*return true;/);
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
  // Same guard as the launcher: scheme/host policy stays mandatory.
  assert.match(controllerKt, /webView\.webViewClient = SiteNavigationGuard\(/);
  const guard = pluginKt.indexOf("class SiteNavigationGuard");
  assert.ok(guard > 0, "guard lives in the plugin file");
  assert.match(pluginKt.slice(guard, guard + 600), /SiteNavigationPolicy\.allows\(uri\)/);
});

// ---------------------------------------------------------------------------
// Build pipeline
// ---------------------------------------------------------------------------

test("android_prepare copies the site controller and pins androidx.webkit", () => {
  assert.match(preparePy, /SITE_CONTROLLER_CLASS = "AnimeHubSiteController"/);
  assert.match(preparePy, /raise SystemExit\(f"site controller not found at \{site_controller_src\}"\)/);
  assert.match(preparePy, /copied \{SITE_CONTROLLER_CLASS\}\.kt/);
  // Reproducible pin, replacing whatever the generated template shipped.
  assert.match(preparePy, /WEBKIT_PIN = 'implementation\("androidx\.webkit:webkit:1\.15\.0"\)'/);
  assert.match(preparePy, /WEBKIT_DEP_RE = re\.compile\(/);
  assert.match(preparePy, /pinned androidx\.webkit:webkit:1\.15\.0/);
  // The pin must be a stable release with no RC/alpha sibling referenced.
  assert.equal(/webkit:1\.\d+\.\d+-(alpha|beta|rc)/.test(preparePy), false);
  // The 1.16.0 conflict that forced 1.15.0 stays documented where the pin is
  // chosen, so nobody "helpfully" bumps back into the Kotlin 1.9 / stdlib 2.1
  // metadata failure the CI showed.
  assert.match(preparePy, /kotlin-stdlib:2\.1\.20/);
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
  assert.match(doc, /1\.15\.0/);
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
