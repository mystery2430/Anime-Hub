// SPDX-License-Identifier: MIT
/**
 * Behaviour tests for `src/logic/storage_scope.js` — the honest wording behind
 * "clear site data" and the one-time 0.3.4 storage-migration notice (AH-05 /
 * AH-P8, R-03 / R-06).
 *
 * The point of these tests is that a message may not claim more than the code
 * did: on Android the HTTP cache and service workers are NOT profile-scoped and
 * are not cleared, and the legacy localStorage/IndexedDB snapshots are never
 * re-imported. Plus the wiring pins, so the pure helpers cannot be orphaned.
 */
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  MIGRATION_NOTICE,
  MIGRATION_NOTICE_KEY,
  clearResultMessage,
  takeMigrationNotice,
} from "../src/logic/storage_scope.js";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (p) => readFileSync(join(ROOT, p), "utf8").replace(/\r\n/g, "\n");

/** Minimal localStorage double that records writes. */
function fakeStorage({ failRead = false, failWrite = false } = {}) {
  const map = new Map();
  return {
    map,
    getItem(key) {
      if (failRead) throw new Error("storage kapalı");
      return map.has(key) ? map.get(key) : null;
    },
    setItem(key, value) {
      if (failWrite) throw new Error("kota dolu");
      map.set(key, value);
    },
  };
}

test("the desktop message states the whole-profile scope", () => {
  for (const platform of ["windows", "macos", "linux"]) {
    const msg = clearResultMessage(platform);
    assert.match(msg, /profil dizini/, `${platform}: must name the profile directory`);
    assert.match(msg, /önbelle/i, `${platform}: the directory really does contain the cache`);
    assert.match(msg, /servis çalışanları/, `${platform}: and the service workers`);
    assert.match(msg, /şifreli oturum kayıtları/, `${platform}: plus the encrypted blobs`);
    assert.equal(
      msg.includes("silinemiyor"),
      false,
      `${platform}: no Android caveat on a platform that does delete them`,
    );
  }
});

test("the Android message does not claim the cache or service workers were cleared", () => {
  for (const platform of ["android", "", "haiku", undefined]) {
    const msg = clearResultMessage(platform);
    assert.match(msg, /profil çerezleri/, "cookies were cleared");
    assert.match(msg, /konum izinleri/, "geolocation permissions were cleared");
    assert.match(msg, /web depolama/, "Web Storage was cleared");
    // The caveat, and no overclaim.
    assert.match(msg, /HTTP önbelleği ve servis çalışanları/);
    assert.match(msg, /profil bazında silinemiyor/);
    assert.equal(
      /önbelle[^.]*silindi/i.test(msg),
      false,
      "must never say the cache was deleted",
    );
  }
});

test("an unknown platform gets the conservative Android wording", () => {
  // Overclaiming what was deleted is the failure that matters, so "" (the
  // app_info call failed) must not produce the desktop message.
  assert.equal(clearResultMessage(""), clearResultMessage("android"));
});

test("the migration notice is shown exactly once per storage", () => {
  const storage = fakeStorage();
  assert.equal(takeMigrationNotice(storage), MIGRATION_NOTICE);
  assert.equal(storage.map.get(MIGRATION_NOTICE_KEY) !== undefined, true, "flag must be written");
  assert.equal(takeMigrationNotice(storage), null);
  assert.equal(takeMigrationNotice(storage), null);
  // A different launcher profile (fresh storage) sees it once too.
  assert.equal(takeMigrationNotice(fakeStorage()), MIGRATION_NOTICE);
});

test("the notice text says the snapshots are NOT imported and why", () => {
  assert.match(MIGRATION_NOTICE, /localStorage\/IndexedDB/);
  assert.match(MIGRATION_NOTICE, /içe\s*\n?\s*aktarılmıyor|aktarılmıyor/, "must state non-import");
  assert.match(MIGRATION_NOTICE, /Yalnızca çerezler/, "only cookies were carried over");
  assert.match(MIGRATION_NOTICE, /yeniden giriş/, "and the user consequence");
  assert.equal(
    MIGRATION_NOTICE.includes("taşındı"),
    true,
    "must not imply the snapshots were migrated",
  );
});

test("unusable storage repeats the notice instead of losing it", () => {
  // Fail toward informing: no storage at all, a throwing read, or a throwing
  // write must all still surface the notice, and must not throw.
  assert.equal(takeMigrationNotice(null), MIGRATION_NOTICE);
  const failRead = fakeStorage({ failRead: true });
  assert.equal(takeMigrationNotice(failRead), MIGRATION_NOTICE);
  assert.equal(takeMigrationNotice(failRead), MIGRATION_NOTICE);
  const failWrite = fakeStorage({ failWrite: true });
  assert.equal(takeMigrationNotice(failWrite), MIGRATION_NOTICE);
  // The flag was not remembered, so it repeats rather than vanishing.
  assert.equal(takeMigrationNotice(failWrite), MIGRATION_NOTICE);
});

test("main.js renders the honest scope and the one-time notice", () => {
  const main = read("src/main.js");
  assert.match(
    main,
    /import \{ clearResultMessage, takeMigrationNotice \} from "\.\/logic\/storage_scope\.js";/,
  );
  // The old overclaiming literal is gone.
  assert.equal(main.includes('showNotice("Site verileri temizlendi.", "info")'), false);
  assert.match(main, /showNotice\(clearResultMessage\(currentPlatform\), "info"\)/);
  // The platform is loaded once at boot, before anything renders with it.
  const bootAt = main.indexOf("async function boot() {");
  assert.ok(bootAt > 0, "boot() missing");
  const bootBody = main.slice(bootAt);
  assert.match(bootBody, /await loadPlatform\(\);/);
  assert.match(bootBody, /takeMigrationNotice\(window\.localStorage\)/);
  assert.match(bootBody, /if \(migrationNotice\) showNotice\(migrationNotice, "info"\)/);
  // An unknown platform is "" and never a guessed value.
  assert.match(main, /let currentPlatform = "";/);
  assert.match(main, /currentPlatform = info && typeof info\.platform === "string" \? info\.platform : "";/);
});

test("the new logic module is part of the test run", () => {
  const pkg = JSON.parse(read("package.json"));
  assert.match(pkg.scripts.test, /tests\/storage_scope\.test\.js/, "must be listed in npm test");
  const src = read("src/logic/storage_scope.js");
  assert.ok(src.startsWith("// SPDX-License-Identifier: MIT"), "license header");
});
