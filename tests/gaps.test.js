/**
 * Locks the gaps the handoff called out that can be checked without a device:
 * the real GitHub coordinates, and the Android release workflow actually
 * doing the two manual steps in the right order.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
// Normalise line endings: a Windows checkout may hand us CRLF files.
const read = (p) => readFileSync(p, "utf8").replace(/\r\n/g, "\n");

// GitHub's Windows runners expose `python`, not `python3`.
function pythonBin() {
  const candidates = process.platform === "win32" ? ["python", "python3"] : ["python3", "python"];
  for (const bin of candidates) {
    const probe = spawnSync(bin, ["--version"], { encoding: "utf8" });
    if (probe.status === 0) return bin;
  }
  return candidates[0];
}

test("README points at this repository, not the animehub/animehub placeholder", () => {
  const readme = read(join(ROOT, "README.md"));
  assert.equal(readme.includes("animehub/animehub"), false);
  assert.equal(readme.includes("github.com/animehub/"), false);
  assert.equal(readme.includes("mystery2430/Anime-Hub"), true);
  assert.equal(readme.includes("https://github.com/mystery2430/Anime-Hub.git"), true);
});

test("android release workflow prepares the project before signing", () => {
  const yml = read(join(ROOT, ".github/workflows/release-android.yml"));
  for (const line of yml.split("\n")) {
    if (/^\s*if:/.test(line)) {
      assert.equal(
        line.includes("secrets."),
        false,
        `secrets cannot be used in if: (${line.trim()})`,
      );
    }
  }
  const init = yml.indexOf("tauri android init");
  const prepare = yml.indexOf("android_prepare.py");
  const keystore = yml.indexOf("keystore.properties");
  assert.ok(init > 0, "workflow must run tauri android init");
  assert.ok(prepare > init, "Kotlin/PiP prepare must run after init");
  assert.ok(keystore > prepare, "keystore.properties is written only after gen/android exists");
  assert.match(yml, /NDK_HOME/);
  assert.match(yml, /shred -u/);
  assert.match(yml, /platforms;android-36/);
  assert.match(yml, /ndk;29\.0\.13846066/);
  assert.equal(yml.includes("setup-ndk"), false);
});

test("CI android job builds and uploads an installable debug APK", () => {
  const yml = read(join(ROOT, ".github/workflows/ci.yml"));
  assert.match(yml, /platforms;android-36/);
  assert.match(yml, /ndk;29\.0\.13846066/);
  assert.equal(yml.includes("setup-ndk"), false);
  assert.match(yml, /--debug --apk --target aarch64/);
  assert.match(yml, /CARGO_PROFILE_DEV_DEBUG/);
  assert.match(yml, /animehub-android-aarch64-debug/);
  assert.match(yml, /actions\/upload-artifact@v7/);
});

test("opening an Android site no longer shows the storage-isolation toast", () => {
  const ui = read(join(ROOT, "src/main.js"));
  const start = ui.indexOf("async function openSite(tile)");
  const end = ui.indexOf("/** Ask the backend to close", start);
  assert.ok(start >= 0 && end > start, "openSite function should be present");
  const openSite = ui.slice(start, end);
  assert.equal(openSite.includes("opened.isolated"), false);
  assert.equal(openSite.includes("şifreli blob'lara aktarılır"), false);
  assert.match(ui, /aynı kaynaklı localStorage\/IndexedDB yalıtımı henüz çözülmedi/);
});

// The file `tauri android init` writes (crates/tauri-cli/templates/mobile/
// android/app/src/main/MainActivity.kt); android_prepare.py injects code into
// its body, so the real shape matters here.
function mainActivitySource(packageName, baseClass = "TauriActivity()") {
  return `package ${packageName}

import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : ${baseClass} {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }
}
`;
}

function fakeGen(packageName, baseClass) {
  const gen = mkdtempSync(join(tmpdir(), "animehub-android-"));
  const javaDir = join(gen, "app/src/main/java", ...packageName.split("."));
  mkdirSync(javaDir, { recursive: true });
  writeFileSync(join(javaDir, "MainActivity.kt"), mainActivitySource(packageName, baseClass));
  writeFileSync(
    join(gen, "app/src/main/AndroidManifest.xml"),
    `<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application>
        <activity android:name=".MainActivity" android:exported="true">
        </activity>
    </application>
</manifest>
`,
  );
  writeFileSync(
    join(gen, "app/build.gradle.kts"),
    `plugins {
    id("com.android.application")
}

android {
    compileSdk = 36
}
`,
  );
  return gen;
}

test("android_prepare copies the bridge and patches PiP exactly once", () => {
  const gen = fakeGen("dev.animehub.app");
  try {
    const run = () =>
      spawnSync(pythonBin(), ["scripts/android_prepare.py", "--root", ROOT, "--gen", gen], {
        cwd: ROOT,
        encoding: "utf8",
      });
    const first = run();
    assert.equal(first.status, 0, first.stderr || first.stdout);
    const kt = read(join(gen, "app/src/main/java/dev/animehub/app/AnimeHubPlugin.kt"));
    assert.match(kt, /^package dev\.animehub\.app$/m);
    assert.match(kt, /fun keystore_seal/);
    const xml = read(join(gen, "app/src/main/AndroidManifest.xml"));
    assert.match(xml, /supportsPictureInPicture="true"/);
    assert.match(xml, /android:scheme="animehub"/);
    assert.match(xml, /android:host="anilist"/);
    const rules = read(join(gen, "app/proguard-rules.pro"));
    assert.match(rules, /-keep class dev\.animehub\.app\.AnimeHubPlugin/);
    const gradle = read(join(gen, "app/build.gradle.kts"));
    assert.match(gradle, /signingConfigs\.maybeCreate\("release"\)/);
    assert.match(gradle, /keystore\.properties/);

    // The PiP controller is generated from its template with the *same* WebView
    // JS that Rust injects, so both halves cannot drift apart.
    const controller = read(
      join(gen, "app/src/main/java/dev/animehub/app/AnimeHubPipController.kt"),
    );
    assert.match(controller, /object AnimeHubPipController/);
    assert.match(controller, /fun requestEnter\(/);
    assert.equal(
      controller.includes("__ANIMEHUB_PIP_CONTROLLER_JS__"),
      false,
      "the JS placeholder must be filled in",
    );
    const pipJs = read(join(ROOT, "src-tauri/src/web/pip_controller.js"));
    assert.equal(
      controller.includes(pipJs.trim()),
      true,
      "the embedded controller must be byte-identical to the shipped JS",
    );
    assert.match(controller, /setSourceRectHint/);

    // MainActivity: overrides + imports, and the block is marker-delimited.
    const activity = read(join(gen, "app/src/main/java/dev/animehub/app/MainActivity.kt"));
    assert.match(activity, /AnimeHubPipController\.attach\(this, webView\)/);
    assert.match(activity, /override fun onPictureInPictureRequested\(\): Boolean/);
    assert.match(activity, /override fun onUserLeaveHint\(\)/);
    assert.match(activity, /onPictureInPictureModeChanged\(/);
    // WryActivity.onPause() pauses the WebView; PiP must keep it rendering.
    assert.match(activity, /override fun onPause\(\)/);
    assert.match(activity, /keepRenderingInPip\(/);
    // A finished Activity leaves Tauri's per-process state stale: end the process.
    assert.match(activity, /if \(isFinishing\) \{/);
    assert.match(activity, /android\.os\.Process\.killProcess\(android\.os\.Process\.myPid\(\)\)/);
    assert.match(activity, /import android\.content\.res\.Configuration/);
    assert.match(activity, /import android\.webkit\.WebView/);
    assert.match(activity, /class MainActivity : TauriActivity\(\) \{/);

    const second = run();
    assert.equal(second.status, 0, second.stderr || second.stdout);
    const activity2 = read(join(gen, "app/src/main/java/dev/animehub/app/MainActivity.kt"));
    assert.equal(
      activity2.split("override fun onDestroy").length - 1,
      1,
      "a second run must refresh the block, not append a duplicate override",
    );
    assert.equal(activity2.split("AnimeHub PiP lifecycle (injected").length - 1, 1);
    assert.equal(
      activity2.split("onPictureInPictureRequested").length - 1,
      activity.split("onPictureInPictureRequested").length - 1,
    );
    const xml2 = read(join(gen, "app/src/main/AndroidManifest.xml"));
    assert.equal(xml2.split("supportsPictureInPicture").length - 1, 1);
    assert.equal(xml2.split('android:scheme="animehub"').length - 1, 1);
    // signing block injected exactly once (marker pair, not duplicated)
    const gradle2 = read(join(gen, "app/build.gradle.kts"));
    assert.equal(gradle2.split("AnimeHub release signing").length - 1, 2);
  } finally {
    rmSync(gen, { recursive: true, force: true });
  }
});

test("android_prepare refuses a MainActivity the PiP lifecycle cannot hook into", () => {
  const gen = fakeGen("dev.animehub.app", "AppCompatActivity()");
  try {
    const result = spawnSync(
      pythonBin(),
      ["scripts/android_prepare.py", "--root", ROOT, "--gen", gen],
      { cwd: ROOT, encoding: "utf8" },
    );
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /TauriActivity/);
  } finally {
    rmSync(gen, { recursive: true, force: true });
  }
});

test("android_prepare refuses a generated package that would not load", () => {
  const gen = fakeGen("com.example.wrong");
  try {
    const result = spawnSync(
      pythonBin(),
      ["scripts/android_prepare.py", "--root", ROOT, "--gen", gen],
      { cwd: ROOT, encoding: "utf8" },
    );
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /com\.example\.wrong/);
    assert.match(result.stderr, /dev\.animehub\.app/);
  } finally {
    rmSync(gen, { recursive: true, force: true });
  }
});

test("site card menu is a sibling of the card button, never nested inside it", () => {
  const ui = read(join(ROOT, "src/main.js"));
  const start = ui.indexOf("function renderTile(tile)");
  const end = ui.indexOf("function renderAddTile()", start);
  assert.ok(start >= 0 && end > start, "renderTile should be present");
  const body = ui.slice(start, end);

  // The card button must not receive the menu as a child.
  assert.equal(/button\.appendChild\(menu\)/.test(body), false);
  assert.match(body, /wrap\.appendChild\(button\);/);
  assert.match(body, /wrap\.appendChild\(menu\);/);
  // The menu must not open the site, and the card must not open the menu.
  assert.match(body, /menu\.addEventListener\("click", \(\) => siteMenu\(tile\)\);/);
  assert.equal(/stopPropagation/.test(body), false);
  // Every control keeps an accessible name.
  assert.match(body, /menu\.setAttribute\("aria-label"/);
  // The wrapper is what the grid receives.
  assert.match(body, /return wrap;/);
});

test("site card styles target the wrapper for menu visibility", () => {
  const css = read(join(ROOT, "src/styles.css"));
  assert.match(css, /\.tile-wrap:hover \.tile-menu,\s*\.tile-wrap:focus-within \.tile-menu/);
  assert.equal(/\.tile:hover \.tile-menu/.test(css), false);
});

test("browser-mode settings stub matches the Rust Settings defaults", () => {
  const bridge = read(join(ROOT, "src/logic/bridge.js"));
  const rust = read(join(ROOT, "src-tauri/src/commands.rs"));

  const jsStart = bridge.indexOf("settings: {", bridge.indexOf("const stubState"));
  const jsEnd = bridge.indexOf("keyBackend:", jsStart);
  assert.ok(jsStart > 0 && jsEnd > jsStart, "stub settings block should be present");
  const stub = bridge.slice(jsStart, jsEnd);

  const rStart = rust.indexOf("impl Default for Settings");
  const rEnd = rust.indexOf("\n}\n", rStart);
  assert.ok(rStart > 0 && rEnd > rStart, "Rust Settings default should be present");
  const real = rust.slice(rStart, rEnd);

  const pairs = [
    [/blockPopups: (\w+)/, /block_popups: (\w+)/],
    [/injectCosmeticRules: (\w+)/, /inject_cosmetic_rules: (\w+)/],
    [/fullscreenSites: (\w+)/, /fullscreen_sites: (\w+)/],
    [/pipAutoEnter: (\w+)/, /pip_auto_enter: (\w+)/],
    [/developerOptions: (\w+)/, /developer_options: (\w+)/],
  ];
  for (const [jsRe, rustRe] of pairs) {
    const jsVal = stub.match(jsRe)?.[1];
    const rustVal = real.match(rustRe)?.[1];
    assert.ok(jsVal && rustVal, `both sides should define ${jsRe}`);
    assert.equal(jsVal, rustVal, `default mismatch for ${jsRe}`);
  }
  assert.match(stub, /theme: "system"/);
  assert.match(real, /theme: ThemePref::System/);
});

test("every overlay goes through the shared stack: focus return, inert background, Escape", () => {
  const ui = read(join(ROOT, "src/main.js"));
  // One stack for modals and action sheets.
  assert.match(ui, /const overlayStack = \[\];/);
  assert.match(ui, /function showOverlay\(el\)/);
  assert.match(ui, /function hideOverlay\(el\)/);
  assert.match(ui, /entry\.opener\.focus\(\)/);
  assert.match(ui, /child\.inert = top !== null && child !== top;/);
  // The action sheet uses the shared helpers and is labelled.
  const sheet = ui.slice(ui.indexOf("function showActionSheet"), ui.indexOf("async function clearSiteData"));
  assert.match(sheet, /showOverlay\(backdrop\)/);
  assert.match(sheet, /aria-labelledby", "action-sheet-title"/);
  assert.equal(/backdrop\.remove\(\)/.test(sheet), false);
  // Escape closes the top overlay first, and never during IME composition.
  assert.match(ui, /if \(event\.isComposing\) return;/);
  assert.match(ui, /if \(overlayStack\.length > 0\)/);
  assert.match(ui, /trapFocus\(event\);/);
});

test("the Android navigation guard keeps the desktop host rules in step", () => {
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt"));
  const rs = read(join(ROOT, "src-tauri/src/sites/url_policy.rs"));

  // Suffixes the Rust policy refuses (`h.ends_with(".x")`).
  const rustSuffixes = [...rs.matchAll(/h\.ends_with\("(\.[a-z.]+)"\)/g)].map((m) => m[1]);
  const ktBlock = kt.slice(kt.indexOf("BLOCKED_SUFFIXES = listOf("));
  const ktSuffixes = [...ktBlock.slice(0, ktBlock.indexOf(")")).matchAll(/"(\.[a-z.]+)"/g)].map((m) => m[1]);
  assert.ok(rustSuffixes.length >= 5, "Rust suffix list should be found");
  assert.deepEqual([...ktSuffixes].sort(), [...rustSuffixes].sort());

  // Exact host names refused by Rust must be refused by Kotlin too, except the
  // launcher origin names, which the Kotlin policy blocks by suffix instead.
  const rustHosts = [...rs.matchAll(/"(localhost|localhost\.localdomain|ip6-localhost|ip6-loopback|metadata\.google\.internal)"/g)].map((m) => m[1]);
  const ktHosts = kt.slice(kt.indexOf("BLOCKED_HOSTS = setOf("), kt.indexOf("BLOCKED_SUFFIXES")).match(/"[^"]+"/g).map((s) => s.slice(1, -1));
  for (const host of new Set(rustHosts)) assert.ok(ktHosts.includes(host), `Kotlin should refuse ${host}`);

  // The launcher origin has no exception: a site must not load it.
  assert.equal(/tauri\.localhost"\s*->\s*return true/.test(kt), false);
});
