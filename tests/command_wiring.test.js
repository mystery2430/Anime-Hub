import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => readFileSync(join(root, rel), "utf8");

const commandsRs = read("src-tauri/src/commands.rs");
const libRs = read("src-tauri/src/lib.rs");
const mainJs = read("src/main.js");
const bridgeJs = read("src/logic/bridge.js");

/** `#[tauri::command]` functions declared in commands.rs. */
const declared = new Set(
  [...commandsRs.matchAll(/#\[tauri::command(?:\s*\([^)]*\))?\]\s*(?:pub\s+)?(?:async\s+)?fn\s+([a-z_0-9]+)/g)].map(
    (m) => m[1],
  ),
);

/**
 * Commands registered in `tauri::generate_handler![…]`.
 *
 * Scoped to the macro body: `lib.rs` also *calls* other `commands::…` helpers,
 * and an unscoped match would treat those as registrations.
 */
const handlerBlock = /generate_handler!\[([\s\S]*?)\]/.exec(libRs)?.[1] ?? "";
const registered = new Set(
  [...handlerBlock.matchAll(/commands::([a-z_0-9]+)/g)].map((m) => m[1]),
);

test("every registered command is declared in commands.rs", () => {
  assert.ok(registered.size > 20, `expected the handler list, got ${registered.size}`);
  const missing = [...registered].filter((name) => !declared.has(name));
  assert.deepEqual(missing, [], `registered but not declared: ${missing.join(", ")}`);
});

test("every command the launcher invokes is registered", () => {
  const invoked = new Set(
    [...mainJs.matchAll(/call\(\s*["'`]([a-z_0-9]+)["'`]/g)].map((m) => m[1]),
  );
  assert.ok(invoked.size > 0, "expected the launcher to call commands");
  const unregistered = [...invoked].filter((name) => !registered.has(name));
  assert.deepEqual(unregistered, [], `invoked but not registered: ${unregistered.join(", ")}`);
});

// BUG-05: hiding a built-in is a registry flag, not a delete, but `list_sites`
// returns the visible grid only — so `set_site_hidden(id, false)` had no caller
// and «Başlatıcıdan gizle» was a one-way door. These pin the read/write pair.
test("hidden built-ins can be listed and restored from the launcher", () => {
  assert.equal(registered.has("list_hidden_sites"), true);
  assert.equal(declared.has("list_hidden_sites"), true);
  assert.match(commandsRs, /pub\s+fn\s+list_hidden_sites/);
  assert.match(commandsRs, /reg\.hidden\(\)/);
  assert.match(mainJs, /call\(\s*["'`]list_hidden_sites["'`]/);
  assert.match(mainJs, /call\(\s*["'`]set_site_hidden["'`]/);
  // Browser mode must not delete a built-in either.
  assert.match(bridgeJs, /case "list_hidden_sites"/);
  assert.match(bridgeJs, /case "set_site_hidden"/);
});

test("the Android prepare step treats every referenced Kotlin unit as required", () => {
  const prep = read("scripts/android_prepare.py");
  for (const unit of ["IpPolicy", "SiteNavigationPolicy", "SiteBlocklist", "SiteClearFlow"]) {
    assert.equal(
      prep.includes(`"${unit}"`),
      true,
      `${unit}.kt must be part of the prepare step`,
    );
  }
  // A silent "warning" copy used to shrink the compile set, and the failure
  // surfaced later as an unresolved reference inside Gradle.
  assert.equal(
    /warning: (IpPolicy|SiteNavigationPolicy)\.kt not found/.test(prep),
    false,
    "IpPolicy/SiteNavigationPolicy must fail loudly, not warn",
  );
});

// Round 6 (security follow-up) source-level guards. Behaviour that needs a
// device (WebView callbacks, native flush) is covered by the Kotlin unit tests
// for the pure policy code; these pins catch the wiring being undone.
test("blocklist sync is fail-closed and both call sites propagate the failure", () => {
  assert.match(
    commandsRs,
    /fn sync_android_blocklist\(state: &AppState\) -> AppResult<\(\)>/,
    "sync must return a result",
  );
  assert.match(commandsRs, /sync_android_blocklist\(&state\)\?;/);
  const callSites = commandsRs.match(/sync_android_blocklist\(&state\)\??;/g) ?? [];
  assert.equal(
    callSites.some((line) => line.endsWith(");")),
    false,
    "no call site may ignore the transfer result",
  );
  // A poisoned lock must not become an empty (re-enabled-looking) snapshot.
  assert.equal(/blocklist\.lock\(\)[\s\S]{0,80}unwrap_or_default/.test(commandsRs), false);
});

test("OAuth login validates the redirect before writing pending state", () => {
  const loginStart = commandsRs.slice(commandsRs.indexOf("pub async fn anilist_login_start"));
  const validatorAt = loginStart.indexOf("validate_redirect_uri");
  const stateAt = loginStart.indexOf("*s = Some(state_value)");
  assert.ok(validatorAt > 0, "login start must validate the redirect URI");
  assert.ok(stateAt > 0, "login start still writes pending state");
  assert.ok(validatorAt < stateAt, "validation must happen before pending state");
  // The listener is only started for the port the URI actually names.
  assert.match(loginStart, /LoginRedirect::Loopback\(port\)/);
  assert.equal(/loopback_port\(&cfg\.redirect_uri\)/.test(loginStart), false);
});

test("top-level navigation consults the DNS gate and the gate is shipped", () => {
  const guard = read("src-tauri/android-plugin/kotlin/dev/animehub/app/SiteNavigationGuard.kt");
  const gate = read("src-tauri/android-plugin/kotlin/dev/animehub/app/NavigationDnsGate.kt");
  assert.match(guard, /shouldOverrideUrlLoading[\s\S]{0,900}dnsGate\.decide/);
  assert.match(guard, /dnsGate\.cachedVerdict/);
  assert.match(gate, /class NavigationDnsGate/);
  assert.match(gate, /Decision\.BLOCK/);
  assert.match(read("scripts/android_prepare.py"), /"NavigationDnsGate"/);
});

test("flush worker replacement is bounded and can refuse work", () => {
  const ops = read("src-tauri/android-plugin/kotlin/dev/animehub/app/SiteProfileOps.kt");
  const worker = read("src-tauri/android-plugin/kotlin/dev/animehub/app/SiteFlushWorker.kt");
  assert.match(ops, /MAX_ABANDONED_WORKERS: Int = 2/);
  assert.match(ops, /fun replacementAllowed\(abandoned: Int\): Boolean/);
  assert.match(worker, /replacementAllowed\(abandonedWorkers\)/);
  assert.match(worker, /fun execute\([\s\S]{0,200}\): Boolean/);
  assert.match(worker, /flush worker exhausted/);
});

test("Actions pinning is reported as a blocker, not claimed as fixed", () => {
  // R-09 / item 5: the hardened, SHA-pinned workflows live in ci-fixes/ and
  // the ACTIVE workflows still use mutable tags, because this branch's token
  // is refused ("without `workflow` scope"). Bypassing that was explicitly out
  // of scope, so the honest state is: proposals pinned, active files not, and
  // the limit written down. This test fails if the docs start claiming
  // otherwise.
  const docs = read("docs/security-residual-risks.md");
  assert.match(docs, /workflow` scope/);
  assert.match(docs, /Actions pinning/);
  const active = read(".github/workflows/ci.yml");
  assert.equal(
    /uses:\s*actions\/checkout@[0-9a-f]{40}/.test(active),
    false,
    "if the active workflow ever gets pinned, update the blocker docs and this test",
  );
  const proposal = read("ci-fixes/ci.yml");
  assert.match(proposal, /uses:\s*actions\/checkout@[0-9a-f]{40} # v7/);
});
