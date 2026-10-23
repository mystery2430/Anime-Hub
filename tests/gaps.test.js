/**
 * Locks the gaps the handoff called out that can be checked without a device:
 * the real GitHub coordinates, and the Android release workflow actually
 * doing the two manual steps in the right order.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, readdirSync } from "node:fs";
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

test("README and the packaged description describe the profile model, not the 0.3.4 cookie swap", () => {
  // R-03: at f9e0acb the tagline, the architecture table and
  // `bundle.longDescription` still described the model this PR replaced
  // (swapping a shared CookieManager jar between sites, "same-origin
  // localStorage/IndexedDB isolation is unsolved"), while another README
  // section correctly described per-site WebView profiles. Both texts ship to
  // users — the packaged description lands in the .deb/.rpm/AppImage metadata —
  // so a stale claim there is a stale claim in the product.
  const readme = read(join(ROOT, "README.md"));
  const conf = read(join(ROOT, "src-tauri/tauri.conf.json"));
  // Collapse whitespace: the claim must not come back in ANY line wrapping.
  const flat = (text) => text.replace(/\s+/g, " ");
  const readmeFlat = flat(readme);
  const confFlat = flat(conf);

  // The stale sentences, verbatim. These are NEGATIVE pins: if any of them
  // returns (reworded wrapping included), this test fails.
  for (const stale of [
    "Android'de çerez kavanozlarını site geçişlerinde değiştiren",
    "CookieManager çerez takası",
    "Tüm güvenlik politikası **Rust tarafında** uygulanır",
    "Android'in etkin ortak `CookieManager` kavanozu WebView tarafından ayrı yönetilir",
    "Android'de `localStorage`/IndexedDB anlık görüntüleri 5 dakikada bir denenir",
    "Android depolama köprüsü, aynı-origin `localStorage`/IndexedDB verisini sağlayıcı bazında güvenilir biçimde ayırmaz",
  ]) {
    assert.ok(
      !readmeFlat.includes(flat(stale)),
      `README hâlâ kaldırılmış mimariyi anlatıyor: "${stale}"`,
    );
  }
  for (const stale of [
    "Android'de CookieManager çerez kavanozu site geçişlerinde değiştirilir",
    "Aynı kaynaklı localStorage/IndexedDB için sağlayıcılar arası yalıtım henüz çözülmemiştir",
    "Android'in etkin CookieManager kavanozu WebView tarafından ayrı yönetilir",
  ]) {
    assert.ok(
      !confFlat.includes(flat(stale)),
      `paket açıklaması hâlâ kaldırılmış mimariyi anlatıyor: "${stale}"`,
    );
  }
  // A cookie-jar SWAP is not a migration: the one-time 0.3.4 import may be
  // described, the swap may not.
  assert.ok(!readmeFlat.includes("çerez takası"), "README çerez takasından söz etmemeli");
  assert.ok(!confFlat.includes("çerez takası"), "paket açıklaması çerez takasından söz etmemeli");

  // The current model has to be stated in both places, including the limits
  // that are NOT solved (network policy on Android, no device test).
  assert.match(readmeFlat, /her siteyi kendi WebView profilinde açan/);
  assert.match(readmeFlat, /SiteNavigationPolicy/);
  assert.match(readmeFlat, /SiteWebViewSettings/);
  assert.match(readmeFlat, /Android'de liste Rust'tan site WebView'ine açılış ve her ayar\/sıfırlama sonrasında aktarılır/);
  assert.match(readmeFlat, /bağlantı artık denetlenen IP'ye sabitleniyor/);
  assert.match(readmeFlat, /doğrudan bağlantıya\s+düşmez/);
  assert.match(readmeFlat, /NavigationDnsGate[\s\S]{0,80}savunma derinliği/);
  assert.match(readmeFlat, /android-dns-pinning-device-checklist/);
  assert.match(readmeFlat, /Aktarım başarısız olursa yeni site açılmaz/);
  assert.match(readmeFlat, /androidx\.webkit `MULTI_PROFILE`/);
  assert.match(confFlat, /MULTI_PROFILE ile site başına kendi WebView profilinde açılır/);
  assert.match(confFlat, /Android'de kullanıcı engel listesi site WebView'ine aktarılır/);
  assert.match(confFlat, /bağlantı denetlenen IP adresine sabitlenir/);
  assert.match(confFlat, /proxy uygulanamazsa site açılmaz/);
  assert.match(confFlat, /fiziksel bir Android cihazında test edilmemiştir/);

  // The config stays parseable and the deep-link registration the OAuth
  // callback check compares against is untouched by the wording fix.
  const parsed = JSON.parse(conf);
  // No Android custom-scheme intent filter: another app can claim it and
  // receive both the OAuth code and the state. Mobile login is loopback-only.
  assert.deepEqual(parsed.plugins["deep-link"].mobile, []);
  assert.ok(parsed.plugins["deep-link"].desktop.schemes.includes("animehub"));
  assert.ok(parsed.bundle.longDescription.length > 200);
  assert.ok(parsed.bundle.shortDescription.length > 10);
});

test("the declared MSRV is one value, and CI actually builds with it", () => {
  // The manifests said 1.77 for a long time while the dependency graph needed
  // 1.77.2 (tauri) and, after the RUSTSEC-2026-0385 fix, 1.87 (ctutils). A
  // declared MSRV nobody builds with is a claim, not a gate.
  const version = (text) => {
    const match = text.match(/^rust-version = "([^"]+)"/m);
    return match ? match[1] : null;
  };
  const main = version(read("src-tauri/Cargo.toml"));
  const plugin = version(read("src-tauri/android-plugin/Cargo.toml"));
  assert.ok(main, "src-tauri/Cargo.toml must declare rust-version");
  assert.equal(main, plugin, "both manifests must declare the same MSRV");

  // The lock is the real constraint: every crate that carries the RustCrypto
  // 0.11/0.2 line needs 1.85+ and ctutils 0.4.5 needs 1.87.
  const lock = read("src-tauri/Cargo.lock");
  const ctutils = lock.match(/name = "ctutils"\nversion = "([^"]+)"/);
  assert.ok(ctutils, "ctutils must be in the lock");
  assert.ok(
    /^0\.4\.(4|5|[6-9])$/.test(ctutils[1]),
    `ctutils ${ctutils[1]} is vulnerable to RUSTSEC-2026-0385`,
  );
  // 1.89 is what the graph needs (aes 0.9.3); the CI gate reads this same
  // value, so a dependency bump that outgrows it fails on the PR.
  const [major, minor] = main.split(".").map(Number);
  assert.ok(
    major > 1 || (major === 1 && minor >= 89),
    `declared MSRV ${main} is below what the locked graph needs (1.89)`,
  );

  const ci = read(".github/workflows/ci.yml");
  assert.match(ci, /name: MSRV \(declared rust-version\)/);
  assert.match(ci, /sed -n 's\/\^rust-version/, "the step reads the manifest");
  assert.match(ci, /cargo "\+\$msrv" check --locked --all-targets/);
  // The README must not promise a lower number than the manifest.
  const readme = read("README.md");
  assert.ok(
    readme.includes(`MSRV \`${main}\``) || readme.includes(`MSRV **\`${main}\`**`),
    `README must state MSRV ${main}`,
  );
  assert.ok(!readme.includes("MSRV `1.77`"), "the 1.77 promise must be gone");
  assert.ok(!ci.includes('"-D warnings" --all-targets > "$RUNNER_TEMP/clippy.log" 2>&1 || true'));
});

test("every external GitHub Action is pinned to a commit SHA, not a mutable tag", () => {
  // The hardened copies live in ci-fixes/ because the branch token has no
  // workflow scope; apply.sh installs them. Validate the proposed files here.
  const dir = join(ROOT, "ci-fixes");
  const files = readdirSync(dir).filter((name) => name.endsWith(".yml"));
  assert.ok(files.length > 0);
  let external = 0;
  for (const name of files) {
    const src = read(join(dir, name));
    for (const line of src.split("\n")) {
      const match = line.match(/^\s*(?:-\s*)?uses:\s*(\S+)\s*(?:#\s*(\S+))?\s*$/);
      if (!match) continue;
      const ref = match[1];
      if (ref.startsWith("./")) continue;
      external += 1;
      const [action, sha] = ref.split("@");
      assert.ok(action && action.includes("/"), `${name}: malformed action ${ref}`);
      assert.match(sha || "", /^[0-9a-f]{40}$/, `${name}: ${action} must be pinned to a commit SHA`);
      assert.match(match[2] || "", /^(?:v?\d|stable$)/, `${name}: version comment missing for ${action}`);
    }
  }
  assert.ok(external >= 20, `expected the CI/release action set, saw ${external}`);
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
  assert.match(
    read(join(ROOT, "ci-fixes/ci.yml")),
    /actions\/upload-artifact@[0-9a-f]{40} # v7/,
    "the proposed CI pins every action",
  );
});

test("CI Kotlin step copies its test sources and reports the real cargo status", () => {
  const yml = read(join(ROOT, ".github/workflows/ci.yml"));
  // Both behaviour-test sources must reach the Gradle test source set, and a
  // failed copy has to fail the step: `--tests "dev.animehub.app.*Test"`
  // passes when ANY matching class exists, so a silently missing
  // SiteProfileOpsTest.kt would shrink the suite to the policy test while
  // the job stayed green.
  for (const name of [
    "SiteNavigationPolicyTest.kt",
    "SiteProfileOpsTest.kt",
    "SiteWebViewSettingsTest.kt",
  ]) {
    const at = yml.indexOf(
      `cp src-tauri/android-plugin/kotlin/dev/animehub/app/${name}`,
    );
    assert.ok(at > 0, `${name} must be copied into the test source set`);
    const line = yml.slice(at, yml.indexOf("\n", at));
    assert.match(line, /\|\| \{/, `${name}: a failed copy must fail the step`);
    assert.equal(line.includes("|| true"), false, `${name}: never masked`);
  }
  // The shared navigation-policy vectors are a test RESOURCE: the Kotlin suite
  // loads navigation-policy.tsv from the classpath and the Rust suite includes
  // the same file, so a missing copy must fail the step, not shrink the suite.
  const fixtureAt = yml.indexOf("cp tests/fixtures/navigation-policy.tsv");
  assert.ok(fixtureAt > 0, "the shared vectors must reach the test resources");
  const fixtureLine = yml.slice(fixtureAt, yml.indexOf("\n", fixtureAt));
  assert.match(fixtureLine, /src\/test\/resources/, "must land in the test resources");
  assert.match(fixtureLine, /\|\| \{/, "a failed fixture copy must fail the step");
  assert.equal(fixtureLine.includes("|| true"), false, "fixture copy never masked");

  // The tauri-gradle generation build stays non-gating on purpose (the real
  // gates are the Gradle exit code and the APK step), but the status it
  // prints must come from cargo itself: `$?` after `|| true` is always 0 and
  // made a failed generation look green in the log.
  const cargoAt = yml.indexOf(
    "cargo build --manifest-path src-tauri/Cargo.toml --target aarch64-linux-android",
  );
  assert.ok(cargoAt > 0, "the tauri-gradle generation build is missing");
  const cargoLine = yml.slice(cargoAt, yml.indexOf("\n", cargoAt));
  assert.equal(cargoLine.includes("|| true"), false, "cargo status must not be masked");
  const statusAt = yml.indexOf("cargo_status=$?", cargoAt);
  assert.ok(statusAt > cargoAt, "the status is captured right after the build");
  assert.match(yml, /set \+e/, "the step stays non-gating via set +e, not via masking");
  // The real gates keep failing the job on their own.
  assert.match(yml, /BUILD SUCCESSFUL" \/tmp\/kotlin-test\.log \|\| \{ echo "::error::BUILD SUCCESSFUL not found"; exit 1; \}/);
  assert.match(yml, /exit \$status/);
});

test("the obsolete Android storage-isolation warning is gone from the UI", () => {
  const ui = read(join(ROOT, "src/main.js"));
  const start = ui.indexOf("async function openSite(tile)");
  const end = ui.indexOf("export async function backToLauncher", start);
  assert.ok(start >= 0 && end > start, "openSite function should be present");
  const openSite = ui.slice(start, end);
  assert.equal(openSite.includes("opened.isolated"), false);
  assert.equal(openSite.includes("şifreli blob'lara aktarılır"), false);
  // Android now isolates per site through WebView profiles, so the old
  // "same-origin isolation not yet solved" claim must not reappear anywhere,
  // and the flag that surfaced it was removed with the claim.
  assert.equal(
    ui.includes("aynı kaynaklı localStorage/IndexedDB yalıtımı henüz çözülmedi"),
    false,
  );
  assert.equal(ui.includes("androidIsolationNote"), false);
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
  // Root build.gradle.kts as `tauri android init` (CLI 2.11) writes it —
  // kotlin-gradle-plugin 1.9.25 is the pin android_prepare must align.
  writeFileSync(
    join(gen, "build.gradle.kts"),
    `buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:8.11.0")
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:1.9.25")
    }
}
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
    // The site-profile controller is required: AnimeHubPlugin's site_*
    // commands have no other implementation.
    const siteController = read(
      join(gen, "app/src/main/java/dev/animehub/app/AnimeHubSiteController.kt"),
    );
    assert.match(siteController, /^package dev\.animehub\.app$/m);
    assert.match(siteController, /object AnimeHubSiteController/);
    assert.match(siteController, /WebViewFeature\.MULTI_PROFILE/);
    // Reproducible androidx.webkit pin (multi-profile needs it; the fakeGen
    // template ships no webkit line, so this also covers the append path).
    const gradleFile = read(join(gen, "app/build.gradle.kts"));
    assert.match(gradleFile, /implementation\("androidx\.webkit:webkit:1\.16\.0"\)/);
    assert.equal(gradleFile.includes("webkit:1.14.0"), false);
    assert.equal(gradleFile.split("androidx.webkit:webkit:").length, 1 + 1, "exactly one webkit dep");
    // ...and the compiler-side half of the same contract: the generated
    // project's Kotlin Gradle plugin is raised to the kotlin-stdlib family
    // webkit 1.16.0 resolves (2.1.20), so the compiler reading the classpath
    // is the same Kotlin that wrote it. Without this, CI fails with
    // "Module was compiled with an incompatible version of Kotlin".
    const rootGradle = read(join(gen, "build.gradle.kts"));
    assert.match(rootGradle, /classpath\("org\.jetbrains\.kotlin:kotlin-gradle-plugin:2\.1\.20"\)/);
    assert.equal(rootGradle.includes("kotlin-gradle-plugin:1.9.25"), false);
    assert.equal(
      rootGradle.split("kotlin-gradle-plugin:").length,
      1 + 1,
      "exactly one kotlin-gradle-plugin classpath",
    );

    // IpPolicy.kt – saf IP mantigi, APK ve test ortak kullanir
    const ipPolicy = read(join(gen, "app/src/main/java/dev/animehub/app/IpPolicy.kt"));
    assert.match(ipPolicy, /^package dev\.animehub\.app$/m);
    assert.match(ipPolicy, /object IpPolicy/);
    assert.match(ipPolicy, /fun isPublicIp\(/);
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
    // WryActivity.onPause() pauses the launcher WebView; the site view
    // follows the Activity lifecycle, and PiP keeps rendering.
    assert.match(activity, /override fun onPause\(\)/);
    assert.match(activity, /keepRenderingInPip\(/);
    assert.match(activity, /override fun onResume\(\)/);
    assert.match(activity, /AnimeHubSiteController\.onPause\(this\)/);
    assert.match(activity, /AnimeHubSiteController\.onResume\(this\)/);
    // Site view torn down (profile flushed) before PiP unbind on destroy.
    const destroyAt = activity.indexOf("AnimeHubSiteController.destroy(this)");
    const detachAt = activity.indexOf("AnimeHubPipController.detach(this)");
    assert.ok(destroyAt > 0 && destroyAt < detachAt);
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
    const rootGradle2 = read(join(gen, "build.gradle.kts"));
    assert.equal(rootGradle2.split("kotlin-gradle-plugin:").length, 1 + 1, "KGP pin idempotent");
    assert.match(rootGradle2, /kotlin-gradle-plugin:2\.1\.20/);
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
  const ktSite = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/SiteNavigationPolicy.kt"));
  const ktIp = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/IpPolicy.kt"));
  const kt = ktSite + "\n" + ktIp;
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

test("Android navigation policy: numeric IPv4 spellings and documentation ranges are refused like Rust", () => {
  // Static check only. IP logic now lives in IpPolicy.kt ve SiteNavigationPolicy.kt (saf, Android bagimliligi yok)
  // ve her iki dosya da kontrol edilir.
  const ktPlugin = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt"));
  const ktPolicy = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/IpPolicy.kt"));
  const ktSite = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/SiteNavigationPolicy.kt"));
  const kt = ktPlugin + "\n" + ktPolicy + "\n" + ktSite;
  // Numeric host forms now go through the WHATWG IPv4 parser in IpPolicy, the
  // same algorithm the desktop `url` crate runs: `010.0.0.1` is 8.0.0.1 (public)
  // while `0177.0.0.1` is 127.0.0.1 (loopback). A regex on the last label cannot
  // express that, and Android's Uri does not normalise hosts for us.
  assert.match(ktPolicy, /fun parseIpv4\(host: String\): String\?/);
  assert.match(ktPolicy, /fun endsInANumber\(host: String\): Boolean/);
  assert.match(ktSite, /if \(IpPolicy\.endsInANumber\(bare\)\) \{/);
  assert.match(ktSite, /val canonical = IpPolicy\.parseIpv4\(bare\) \?: return false/);
  assert.equal(/NUMERIC_LABEL/.test(kt), false, "the old last-label regex must be gone");
  // ... and the Rust side uses the same parser instead of its own rule.
  const rs = read(join(ROOT, "src-tauri/src/sites/url_policy.rs"));
  assert.match(rs, /fn ends_in_a_number\(host: &str\) -> bool/);
  // (clippy: `bare` is already a &str, so no extra reference)
  assert.match(rs, /if ends_in_a_number\(bare\)/);
  // Documentation ranges, as Rust url_policy::is_public_ip rejects them – now in IpPolicy.kt
  assert.match(kt, /\(a == 198 && b == 51 && c == 100\)/);
  assert.match(kt, /\(a == 203 && b == 0 && c == 113\)/);
  // IpPolicy.kt'nin APK ve test tarafindan ortak kullanildigini dogrula
  assert.match(ktPolicy, /object IpPolicy/);
  assert.match(ktPolicy, /fun isPublicIp\(/);
});

test("Android IPC is limited to the launcher origins and main frame, never a site-facing JavascriptInterface", () => {
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt"));
  // Removes wry's `ipc` object before the listener is registered under the same name.
  assert.match(kt, /webView\.removeJavascriptInterface\(LauncherIpc\.OBJECT_NAME\)/);
  const remove = kt.indexOf("removeJavascriptInterface(LauncherIpc");
  const add = kt.indexOf("WebViewCompat.addWebMessageListener(");
  assert.ok(remove > 0 && add > remove, "remove must run before addWebMessageListener");
  // Only the launcher origins may receive the object.
  assert.match(kt, /setOf\("http:\/\/tauri\.localhost", "https:\/\/tauri\.localhost"\)/);
  // Main-frame guard on the message path.
  assert.match(kt, /if \(!isMainFrame\) return/);
  // The object is removed on every path, before any early return, so an
  // unsupported or non-Rust WebView still gives sites no `ipc` object.
  const removeAt = kt.indexOf("removeJavascriptInterface(LauncherIpc.OBJECT_NAME)");
  const restrictStart = kt.indexOf("private fun restrictIpcToLauncher");
  const firstReturn = kt.indexOf("return", restrictStart);
  assert.ok(removeAt > restrictStart && removeAt < firstReturn, "removal must come before any early return");
  assert.match(kt, /WebViewFeature\.isFeatureSupported\(WebViewFeature\.WEB_MESSAGE_LISTENER\)/);
  assert.doesNotMatch(kt, /wry interface kept/);
  // A failed install is logged as an error and never falls back to a site-visible object.
  assert.match(kt, /try \{\s*WebViewCompat\.addWebMessageListener[\s\S]*?\} catch \(e: Exception\) \{[\s\S]*?Log\.e\(LOG_TAG/);
  // Nothing in the plugin exposes a JavascriptInterface to pages.
  assert.doesNotMatch(kt, /@JavascriptInterface/);
  assert.doesNotMatch(kt, /addJavascriptInterface/);
  // Restriction runs in load(), after the navigation guard is installed.
  const guard = kt.indexOf("installNavigationGuard(webView)");
  const restrict = kt.indexOf("restrictIpcToLauncher(webView)");
  assert.ok(guard > 0 && restrict > guard, "restrictIpcToLauncher must run after installNavigationGuard");
});

test("Android diagnostics: bounded structured events are packaged and exposed through the plugin", () => {
  const plugin = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt"));
  const pip = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPipController.kt"));
  const debugLog = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubDebugLog.kt"));
  const controller = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubSiteController.kt"));
  const prepare = read(join(ROOT, "scripts/android_prepare.py"));
  const rs = read(join(ROOT, "src-tauri/android-plugin/src/lib.rs"));

  assert.match(debugLog, /^object AnimeHubDebugLog \{/m);
  assert.match(debugLog, /const val MAX_EVENTS: Int = 2_000/);
  assert.match(debugLog, /private val events = ArrayDeque<DebugEvent>\(\)/);
  assert.match(debugLog, /private val areas = setOf\(/);
  assert.match(debugLog, /private val phases = setOf\(/);
  assert.match(debugLog, /fun safeOperationId\(/);
  assert.match(prepare, /"AnimeHubDebugLog"/, "the ring must be packaged into the APK");
  assert.match(plugin, /@Command\s+fun debug_log\(/);
  assert.match(plugin, /payload\.put\("droppedEvents", snapshot\.droppedEvents\)/);
  assert.match(plugin, /payload\.put\("capacity", snapshot\.capacity\)/);
  assert.match(rs, /CMD_DEBUG_LOG: &str = "debug_log"/);
  assert.match(rs, /CMD_DEBUG_LOG_RESET: &str = "debug_log_reset"/);

  const sources = [plugin, pip, controller, debugLog].join("\n");
  assert.ok((sources.match(/AnimeHubDebugLog\.record\(/g) || []).length >= 40);
  assert.doesNotMatch(sources, /AnimeHubDebugLog\.add\(/, "the old free-form text logger is gone");
  assert.doesNotMatch(debugLog, /FileOutputStream|FileWriter|HttpURLConnection|fetch\(/);
});

test("PiP play/pause button: shown only for a <video> candidate, never for an iframe", () => {
  // Static check: the JS playback() only controls <video>, so the button must
  // not be offered for an iframe candidate. Not a device test.
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPipController.kt"));
  assert.match(kt, /controllable = answer\.optString\("kind"\) == "video"/);
  assert.match(kt, /if \(controllable\) playbackActions\(activity, playing = true\) else emptyList<RemoteAction>\(\)/);
});

test("PiP play/pause button: only a user press toggles playback, and the receiver stays app-private", () => {
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPipController.kt"));
  assert.match(kt, /const val ACTION_PIP_TOGGLE = "dev\.animehub\.app\.PIP_TOGGLE"/);
  // The button is offered on entry only for a controllable (video) candidate;
  // updatePlaybackAction still shows it in the setActions call for both states.
  assert.match(kt, /if \(controllable\) playbackActions\(activity, playing = true\) else emptyList<RemoteAction>\(\)/);
  assert.match(kt, /\.setActions\(playbackActions\(activity, playing\)\)/);
  assert.match(kt, /Context\.RECEIVER_NOT_EXPORTED/);
  assert.doesNotMatch(kt, /ContextCompat/);
  assert.match(kt, /PendingIntent\.FLAG_IMMUTABLE/);
  assert.match(kt, /\.setPackage\(activity\.packageName\)/);
  // The "toggle" command is sent from exactly one place: the press handler.
  const toggleSends = kt.match(/playbackScript\("toggle"\)/g) || [];
  assert.equal(toggleSends.length, 1, "toggle must have a single call site");
  const handler = kt.slice(kt.indexOf("private fun togglePlayback()"));
  assert.ok(
    handler.indexOf('playbackScript("toggle")') < handler.indexOf("applyPlaybackState"),
    "toggle sits in the press handler",
  );
  // Only the receiver calls the press handler (one definition, one call).
  assert.equal((kt.match(/togglePlayback\(\)/g) || []).length, 2, "one definition and one receiver call");
  // The state read never changes playback.
  assert.match(kt, /playbackScript\("state"\)/);
  // The JS side exposes the function this Kotlin expects, with no $ in it.
  const js = read(join(ROOT, "src-tauri/src/web/pip_controller.js"));
  assert.match(js, /window\[PLAYBACK_FN\] = playback;/);
  assert.doesNotMatch(js, /\$/);
});

test("launcher IPC: the UI-thread listener never calls Rust directly (no main-thread deadlock)", () => {
  // onPostMessage is @UiThread. Rust runs blocking commands inline, and a plugin
  // command waits for the UI thread, so a direct Rust.ipc call there can deadlock.
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt"));
  const start = kt.indexOf("class LauncherIpc(");
  assert.ok(start >= 0, "LauncherIpc must exist");
  const end = kt.indexOf("\n}\n", start);
  const body = kt.slice(start, end);
  assert.match(body, /IPC_EXECUTOR\.execute \{/);
  assert.match(body, /Rust\.ipc\(webViewId, url, body\)/);
  // The only direct Rust.ipc call is the one inside the executor.
  const rustCalls = body.match(/Rust\.ipc\(/g) || [];
  assert.equal(rustCalls.length, 1);
  assert.match(kt, /Executors\.newSingleThreadExecutor/);
});

test("plugin-calling commands use the async runtime, not the IPC dispatch thread", () => {
  // Static check. Tauri runs a blocking command inline on the dispatch thread;
  // these three wait for a Kotlin reply, so they must be marked async.
  const rs = read(join(ROOT, "src-tauri/src/commands.rs"));
  for (const name of ["enter_pip", "set_pip_auto_enter", "debug_log"]) {
    assert.match(rs, new RegExp(`#\\[tauri::command\\(async\\)\\]\\npub fn ${name}\\(`), name);
  }
});

test("fail-closed launcher IPC shows a native notice on every failure path", () => {
  // Static check of AnimeHubPlugin.kt. Each of the three "IPC is off" branches
  // must call the native Toast, because JS cannot show it in that state.
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt"));
  const calls = kt.match(/showIpcOffNotice\(\)/g) || [];
  assert.equal(calls.length, 4, "one definition and three calls");
  assert.match(kt, /import android\.widget\.Toast/);
  assert.match(kt, /activity\.runOnUiThread \{[\s\S]*?Toast\.makeText\(/);
  const start = kt.indexOf("private fun showIpcOffNotice");
  const body = kt.slice(start, kt.indexOf("\n  }\n", start));
  assert.ok(start > 0 && body.length > 0, "function body found");
  assert.doesNotMatch(body, /\$\{/, "fixed text only, no template values");
});

test("the back overlay is installed only on desktop; Android never produces close-site", () => {
  // Static check. If the overlay is ever added to the Android path, the Android
  // close-site handling must be built first (see docs/android-navigation.md).
  const win = read(join(ROOT, "src-tauri/src/web/windows.rs"));
  const evals = win.match(/webview\.eval\(crate::web::session::OVERLAY_SNIPPET\)/g) || [];
  assert.equal(evals.length, 1);
  const mobileStart = win.indexOf("fn open_on_mobile(");
  assert.ok(mobileStart > 0);
  const mobileBody = win.slice(mobileStart, win.indexOf("\n}\n", mobileStart));
  assert.doesNotMatch(mobileBody, /OVERLAY_SNIPPET/);
});

test("the PiP diagnostic report goes to the in-app log only, never a Toast over the site", () => {
  const kt = read(join(ROOT, "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPipController.kt"));
  const start = kt.indexOf("fun showDebugReport(");
  assert.ok(start > 0, "showDebugReport found");
  const body = kt.slice(start, kt.indexOf("\n  }\n", start));
  assert.doesNotMatch(body, /Toast/);
  assert.match(body, /"pip_playback"/);
  assert.match(body, /event\(/);
  const prepare = read(join(ROOT, "scripts/android_prepare.py"));
  assert.match(prepare, /AnimeHubPipController\.showDebugReport\(AnimeHubPipController\.webView\(\)\)/);
});

test("Android-only settings are marked and hidden by platform in main.js", () => {
  const html = read(join(ROOT, "index.html"));
  assert.match(html, /<label class="switch" data-platform="android">\s*<input type="checkbox" id="set-pip"/);
  assert.match(html, /<section data-platform="android">\s*<h3>Geliştirici seçenekleri<\/h3>/);
  const main = read(join(ROOT, "src/main.js"));
  assert.match(main, /async function applyPlatformVisibility\(\)/);
  assert.match(main, /node\.dataset\.platform === "android" && isAndroid/);
  assert.match(main, /await applyPlatformVisibility\(\);\n\s*const s = await call\("get_settings"\)/);
});

test("CI keeps no workflow-wide token, no PR-comment dump and no masked prep failure", () => {
  const yml = read(join(ROOT, ".github/workflows/ci.yml"));
  // Negative assertions run against the file with full-line comments stripped:
  // this workflow explains (in comments) exactly what it no longer does, and a
  // comment mentioning `pull-requests: write` must not satisfy the check.
  const code = yml
    .split("\n")
    .filter((line) => !/^\s*#/.test(line))
    .join("\n");

  // AH-09 / AH-P6: a build-and-test workflow needs no write token. Exporting
  // `github.token` to every step of every job meant a compromised build step on
  // a fork PR held a token that could comment on (and with pull-requests:write,
  // edit around) the pull request.
  assert.equal(
    /GH_TOKEN:\s*\$\{\{\s*github\.token\s*\}\}/.test(code),
    false,
    "no workflow-level GH_TOKEN",
  );
  assert.equal(/pull-requests:\s*write/.test(code), false, "no pull-requests: write");
  assert.match(yml, /permissions:\n  contents: read/, "least privilege stays explicit");
  // The release workflows set their own step-scoped token; this one must not.
  assert.equal(/\bgh pr comment\b/.test(code), false, "no PR-comment dump");
  assert.equal(code.includes("DEBUG123"), false, "the debug dump marker is gone");
  assert.match(code, /GITHUB_STEP_SUMMARY/, "diagnostics go to the step summary");

  // A mandatory prep step must keep its own exit code: `| head ... || true`
  // reported head's status, so a failed android_prepare.py looked green and the
  // APK step built without the Tauri plugin sources.
  assert.equal(
    /python3 scripts\/android_prepare\.py[^\n]*\|\s*head[^\n]*\|\|\s*true/.test(code),
    false,
    "android_prepare.py must not be piped into a masked head",
  );
  assert.match(code, /prepare_status=\$\?/, "python's own status is captured");
  assert.match(code, /exit "\$prepare_status"/, "and it fails the step");
  // The Gradle verdict still wins when both fail: the test result is reported
  // before the restore status is checked.
  const testVerdict = code.indexOf("if [ $status -ne 0 ]; then");
  const prepVerdict = code.indexOf('if [ "$prepare_status" -ne 0 ]; then');
  assert.ok(testVerdict > 0, "the Gradle verdict is still checked");
  assert.ok(prepVerdict > testVerdict, "test failure is reported first");

  // No sandbox-only absolute path may survive into a runner script.
  assert.equal(code.includes("/home/user/"), false, "runner scripts must use relative paths");

  // ... and nothing is skipped to make the job green.
  assert.equal(/continue-on-error:\s*true/.test(code), false, "no continue-on-error");
});

test("CI restores the Kotlin sources it moved out, from a path that exists", () => {
  const yml = read(join(ROOT, ".github/workflows/ci.yml"));
  const code = yml
    .split("\n")
    .filter((line) => !/^\s*#/.test(line))
    .join("\n");

  // The unit-test step cd's into the generated project for Gradle. Everything
  // after that uses repo-relative paths, so it has to come back: the restore
  // used to run from src-tauri/gen/android, where the MainActivity path does not
  // exist and android_prepare.py cannot even be opened (exit 2), and `| head
  // || true` hid both.
  const cdIn = code.indexOf("cd src-tauri/gen/android");
  const cdBack = code.indexOf('cd "$GITHUB_WORKSPACE"');
  assert.ok(cdIn > 0, "the Gradle cd is missing");
  assert.ok(cdBack > cdIn, "the step must return to the workspace root");
  const prepareAt = code.indexOf("prepare_status=$?");
  assert.ok(prepareAt > cdBack, "android_prepare.py must run from the workspace root");

  // The APK step re-runs prepare through an absolute path and verifies the
  // plugin sources are really in the generated source set. This is the last
  // place it can be fixed: an APK built without AnimeHubPlugin.kt compiles and
  // links fine, and only fails at runtime on a device.
  const apkAt = code.indexOf("- name: Build installable debug APK");
  assert.ok(apkAt > 0, "the APK step is missing");
  const apkStep = code.slice(apkAt, code.indexOf("- name:", apkAt + 10));
  assert.match(apkStep, /python3 "\$GITHUB_WORKSPACE\/scripts\/android_prepare\.py"/);
  assert.match(apkStep, /for required in AnimeHubPlugin /);
  assert.match(apkStep, /is missing from the generated source set/);
  assert.match(apkStep, /exit 1/, "a missing source must fail the step");
  assert.equal(
    /android_prepare\.py[^\n]*\|\|\s*true/.test(apkStep),
    false,
    "the APK-step prepare must never be masked",
  );
});
