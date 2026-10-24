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
test("blocklist sync is fail-closed for opens, settings and reset", () => {
  assert.match(
    commandsRs,
    /fn sync_android_blocklist\(app: &AppHandle, state: &AppState\) -> AppResult<\(\)>/,
    "sync must take the app handle (to close a live view) and return a result",
  );
  const callSites = commandsRs.match(/sync_android_blocklist\(&app, &state\)\??;/g) ?? [];
  assert.equal(callSites.length, 3, "open, settings and reset must all transfer the snapshot");
  assert.equal(
    callSites.some((line) => line.endsWith(");")),
    false,
    "no call site may ignore the transfer result",
  );
  assert.match(commandsRs, /fn transfer_blocklist_snapshot/);
  assert.match(commandsRs, /Err\(push_error\) => match close\(\)/);
  assert.match(commandsRs, /close_active_android_site\(app\)/);
  assert.match(
    commandsRs,
    /blocklist_transfer_close_failure_requires_a_restart_not_success/,
    "a close failure must be proven to surface as an error, not success",
  );
  assert.match(commandsRs, /pub fn reset_blocklist\(app: AppHandle/);
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

test("all active GitHub Actions workflows use the reviewed SHA pins", () => {
  const docs = read("docs/security-residual-risks.md");
  const status = read("docs/audit-item-status.md");
  assert.match(docs, /Round 16\.4 — active workflow Actions are SHA-pinned/);
  assert.match(status, /\|\s*\*\*R-09\*\*.*Action pinning implemented/);

  const paths = [
    ".github/workflows/ci.yml",
    ".github/workflows/release-android.yml",
    ".github/workflows/release-desktop.yml",
    ".github/workflows/publish-release.yml",
  ];
  const allActive = paths.map(read).join("\n");
  const verified = {
    "actions/checkout": ["3d3c42e5aac5ba805825da76410c181273ba90b1", "v7"],
    "dtolnay/rust-toolchain": ["b725eda525362e7cf34af9a7243cac1e3f01a7ec", "stable"],
    "Swatinem/rust-cache": ["d1527c8f67ef9af357290475d4af4b0fab8f12e0", "v2"],
    "actions/setup-node": ["949feb2413d6458794dcd2491c4babbbce0c15c1", "v7"],
    "actions/setup-java": ["b843ebf9b913146b38da2eaa7aeddf76074ae1ad", "v6"],
    "android-actions/setup-android": ["be39fa834029ff78f1a44aa3bb0819b8fc2bd8fd", "v4"],
    "actions/upload-artifact": ["cf430e030ddbb5b0abf93d22962f4752f3646cd9", "v7"],
    "actions/download-artifact": ["9000827ccba6bdab643e8b6fd33ac0654aef8333", "v8"],
    "tauri-apps/tauri-action": ["1deb371b0cd8bd54025b384f1cd735e725c4060f", "v1"],
  };
  let external = 0;
  for (const path of paths) {
    const src = read(path);
    for (const line of src.split("\n")) {
      const match = line.match(/^\s*(?:-\s*)?uses:\s*(\S+)/);
      if (!match || match[1].startsWith("./")) continue;
      external += 1;
      const [action, sha] = match[1].split("@");
      const expected = verified[action];
      assert.ok(expected, path + ": unreviewed external Action " + action);
      assert.equal(sha, expected[0], path + ": " + action + " must use the reviewed commit SHA");
      assert.equal((line.split("#", 2)[1] || "").trim(), expected[1], path + ": " + action + " must retain the reviewed version comment");
    }
  }
  assert.equal(external, 36, `expected 36 pinned external Actions, found ${external}`);

  for (const [action, [sha, tag]] of Object.entries(verified)) {
    assert.match(sha, /^[0-9a-f]{40}$/, `${action}: not a full commit SHA`);
    const escapedAction = action.replace(/\//g, "\\/");
    assert.match(
      allActive,
      new RegExp(`uses:\\s*${escapedAction}@${sha}\\s+#\\s*${tag}`),
      `${action}@${tag} SHA must remain present in active workflows`,
    );
    assert.ok(docs.includes(sha) && docs.includes(action), `${action}@${tag} SHA should remain documented`);
  }

  // The ci-fixes copy is a reference; active workflows are validated above.
  assert.match(read("ci-fixes/ci.yml"), /actions\/checkout@[0-9a-f]{40} # v7/);
});

test("blocklist transfers are serialized and generation-guarded (PR #27 follow-up)", () => {
  // The behaviour itself is covered by the Rust tests in commands.rs
  // (`concurrent_out_of_order_transfers_never_restore_a_stale_snapshot`,
  // `stale_blocklist_snapshot_is_refused_without_closing_a_live_site`, …).
  // This is the wiring tripwire: it fails if a later edit drops the guard or
  // routes a transfer around it while leaving those tests behind.
  assert.match(commandsRs, /pub struct BlocklistSyncGate/);
  assert.match(commandsRs, /fn transfer_blocklist_with_generation</);

  // Every Android transfer must go through the guard, never around it.
  const fromSync = commandsRs.slice(
    commandsRs.indexOf("pub(crate) fn sync_android_blocklist"),
  );
  const androidBranch = fromSync.slice(0, fromSync.indexOf("#[cfg(not(target_os"));
  assert.match(
    androidBranch,
    /transfer_blocklist_with_generation\(/,
    "the Android path must use the generation-guarded transfer",
  );
  assert.equal(
    /transfer_blocklist_snapshot\(/.test(androidBranch),
    false,
    "sync_android_blocklist must not call the unguarded helper directly",
  );

  // All three commands still propagate the failure (`?`), so a refused
  // transfer can never be reported as success.
  assert.equal(
    (commandsRs.match(/sync_android_blocklist\(&app, &state\)\?;/g) || []).length,
    3,
    "open_site, update_settings and reset_blocklist must all propagate",
  );

  // The generation is bumped while the matcher lock is held, otherwise a reader
  // could see new rules under an old generation and skip the transfer.
  assert.match(
    commandsRs,
    /\.generation\s+\.fetch_add\(1, std::sync::atomic::Ordering::SeqCst\);/,
  );
  // gate starts above `applied` so the very first open pushes (Kotlin starts
  // empty and lib.rs does not go through rebuild_blocklist).
  assert.match(commandsRs, /generation: std::sync::atomic::AtomicU64::new\(1\)/);
  assert.match(commandsRs, /applied: std::sync::atomic::AtomicU64::new\(0\)/);

  // Every AppState construction carries the gate, or the crate would not build.
  const windowsRs = read("src-tauri/src/web/windows.rs");
  for (const [src, name] of [
    [commandsRs, "commands.rs"],
    [libRs, "lib.rs"],
    [windowsRs, "windows.rs"],
  ]) {
    const gates = (src.match(/blocklist_sync: /g) || []).length;
    const states = (src.match(/diagnostics_lock: /g) || []).length;
    assert.ok(gates >= 1, `${name}: AppState must carry the blocklist sync gate`);
    assert.equal(
      gates,
      states,
      `${name}: every AppState construction needs blocklist_sync`,
    );
  }
});

test("R-05 and the 'Not closed' list agree about Android OAuth", () => {
  // The claim has to match the shipped configuration: no Android intent filter,
  // desktop scheme retained.
  const conf = JSON.parse(read("src-tauri/tauri.conf.json"));
  assert.deepEqual(conf.plugins["deep-link"].mobile, []);
  assert.ok(conf.plugins["deep-link"].desktop.schemes.includes("animehub"));

  const audit = read("docs/audit-item-status.md");
  const risks = read("docs/security-residual-risks.md");

  // An earlier round said another app can register the scheme "and receive the
  // intent" on Android, contradicting both the R-05 row and this config.
  // The correction quotes the wording it replaced, so strip quoted spans before
  // checking: the phrase may only survive as a quotation, never as a claim.
  const unquoted = audit.replace(/"[^"]*"/g, "");
  assert.equal(
    /and receive the intent/.test(unquoted),
    false,
    "Android sends no OAuth intent to a custom scheme: mobile deep-link is []",
  );
  assert.match(
    audit,
    /and receive the intent/,
    "the correction must name the wording it replaced",
  );
  assert.equal(
    /\(R-04, R-05\)/.test(audit),
    false,
    "R-05 is fixed for Android; only R-04 stays unresolved on the lost wording",
  );
  assert.match(audit, /scheme squatting/);
  assert.match(audit, /Android only/);
  assert.match(risks, /there is no OAuth intent for it to receive/);

  // And the ordering the row asserts is real: refuse on Android, then validate
  // the redirect, and only then write the pending state.
  const loginStart = commandsRs.slice(
    commandsRs.indexOf("pub async fn anilist_login_start"),
  );
  const refuseAt = loginStart.indexOf("mobile_native_login_allowed");
  const validateAt = loginStart.indexOf("validate_redirect_uri");
  const stateWriteAt = loginStart.indexOf("*s = Some(state_value)");
  assert.ok(refuseAt > 0, "the Android custom-scheme refusal must exist");
  assert.ok(
    validateAt > refuseAt && stateWriteAt > validateAt,
    "refusal and redirect validation must both precede the pending-state write",
  );
  assert.match(
    loginStart.slice(validateAt, validateAt + 120),
    /cfg!\(desktop\)/,
    "allow_custom_scheme is desktop-only, so Android refuses the scheme",
  );
});
