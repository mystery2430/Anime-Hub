// SPDX-License-Identifier: MIT
/**
 * What "clear site data" and the 0.3.4 storage migration REALLY do, as pure
 * decisions the UI renders and the tests pin.
 *
 * WHY THIS EXISTS
 * ---------------
 * The UI used to say "Site verileri temizlendi." on every platform. That is
 * true on desktop — `clear_site_data` removes the whole per-site profile
 * directory (cookies, DOM storage, IndexedDB, HTTP cache and service workers
 * all live inside it) plus the encrypted session blobs — and only half true on
 * Android, where the profile's cookie jar, geolocation permissions and Web
 * Storage are cleared but Chromium offers no profile-scoped HTTP-cache or
 * service-worker wipe. `WebView.clearCache(true)` is app-wide, so calling it
 * from a site operation would also wipe the launcher's cache, which the site
 * isolation contract forbids. The honest fix is to say what happened; claiming
 * more would be a lie the user cannot check.
 *
 * Same story for the 0.3.4 migration: the legacy `localstorage-<site>` and
 * `indexeddb-<site>` snapshots are deliberately NOT re-imported (the dump was
 * lossy and the import had an origin TOCTOU), only the cookie jar is imported
 * once. That is documented in `docs/android-storage-isolation.md`, but a doc
 * nobody opens is not a notice — so the app says it once, in the UI.
 */

/** Flag key for the one-time migration notice (launcher-scoped localStorage). */
export const MIGRATION_NOTICE_KEY = "animehub.storage-migration-notice";

/** Platforms whose per-site profile directory is deleted as a whole. */
const DESKTOP_PLATFORMS = new Set(["windows", "macos", "linux"]);

/**
 * The notice shown after a successful `clear_site_data`.
 *
 * @param {string} platform `app_info().platform`. Anything unrecognised (or
 *   empty, when the call failed) is answered with the conservative Android
 *   wording: overclaiming what was deleted is the failure mode that matters.
 */
export function clearResultMessage(platform) {
  if (DESKTOP_PLATFORMS.has(platform)) {
    return (
      "Site verileri temizlendi: site profil dizini (çerezler, localStorage/IndexedDB, " +
      "HTTP önbelleği, servis çalışanları) ve şifreli oturum kayıtları silindi."
    );
  }
  return (
    "Site verileri temizlendi: profil çerezleri, konum izinleri ve web depolama " +
    "(localStorage/IndexedDB) silindi. Android'de HTTP önbelleği ve servis çalışanları " +
    "profil bazında silinemiyor; site önbellekten içerik gösterebilir."
  );
}

/** The one-time migration notice text. */
export const MIGRATION_NOTICE =
  "0.3.4'ten geçiş: eski sürümlerin localStorage/IndexedDB anlık görüntüleri içe " +
  "aktarılmıyor — aktarım kayıplıydı ve hangi belgeye yazılacağı garanti edilemiyordu. " +
  "Yalnızca çerezler bir kez taşındı; bazı sitelerde yeniden giriş yapmanız gerekebilir.";

/**
 * Consume the one-time migration notice.
 *
 * Returns [MIGRATION_NOTICE] the first time it is called for a given `storage`
 * and `null` afterwards. A `storage` that throws on read is treated as "not yet
 * seen": repeating an honest notice beats silently never showing it, and a
 * write failure is swallowed for the same reason (the flag simply is not
 * remembered).
 *
 * @param {{getItem(key: string): string|null, setItem(key: string, value: string): void}|null} storage
 * @returns {string|null}
 */
export function takeMigrationNotice(storage) {
  if (!storage) return MIGRATION_NOTICE;
  let seen = null;
  try {
    seen = storage.getItem(MIGRATION_NOTICE_KEY);
  } catch {
    return MIGRATION_NOTICE;
  }
  if (seen) return null;
  try {
    storage.setItem(MIGRATION_NOTICE_KEY, new Date().toISOString());
  } catch {
    // Unrememberable: the notice repeats next launch instead of disappearing.
  }
  return MIGRATION_NOTICE;
}
