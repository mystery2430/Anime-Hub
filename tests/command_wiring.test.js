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
