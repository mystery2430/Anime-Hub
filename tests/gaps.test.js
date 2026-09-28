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
const read = (p) => readFileSync(p, "utf8");

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

test("CI android job installs the Tauri NDK instead of a mismatched one", () => {
  const yml = read(join(ROOT, ".github/workflows/ci.yml"));
  assert.match(yml, /platforms;android-36/);
  assert.match(yml, /ndk;29\.0\.13846066/);
  assert.equal(yml.includes("setup-ndk"), false);
  assert.match(yml, /CARGO_PROFILE_RELEASE_LTO/);
});

function fakeGen(packageName) {
  const gen = mkdtempSync(join(tmpdir(), "animehub-android-"));
  const javaDir = join(gen, "app/src/main/java", ...packageName.split("."));
  mkdirSync(javaDir, { recursive: true });
  writeFileSync(
    join(javaDir, "MainActivity.kt"),
    `package ${packageName}\n\nclass MainActivity\n`,
  );
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
    assert.match(gradle, /signingConfigs\.create\("release"\)/);
    assert.match(gradle, /keystore\.properties/);

    const second = run();
    assert.equal(second.status, 0, second.stderr || second.stdout);
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
