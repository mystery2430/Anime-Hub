# Çerez / Oturum / Şifreli Depolama — Tam Yaşam Döngüsü Denetimi

**Tarih:** 2026-10-14 (Europe/Istanbul) — *İkinci tur takip*  
**Dal:** `fix/cookie-session-lifecycle` → PR #26 (bu audit) — `main` 2bee80a üzerine izole dal, PR #21 / #24 ile karıştırılmadı, `main`’e merge yok.  
**Doğrulanan Head’ler:**

| Kaynak | Kısa | Tam SHA | Link |
|--------|------|---------|------|
| `main` | `2bee80a` | `2bee80a1145a9f458d41379b302bd7e7fa519216` | https://github.com/mystery2430/Anime-Hub/commit/2bee80a1145a9f458d41379b302bd7e7fa519216 |
| PR #21 `guvenlik: IANA IP daraltma + PiP/tema yarisi` | `3feee7a` | `3feee7a680a6be51fbd10b2202c8da4bd546f100` | https://github.com/mystery2430/Anime-Hub/pull/21 |
| PR #24 `fix(desktop): investigate first-load session state` | `1f3f82f` | `1f3f82f880408a9d0b653cdce14c4949272f0a24` | https://github.com/mystery2430/Anime-Hub/pull/24 |
| PR #26 `fix/cookie-session-lifecycle` ilk audit head | `7aeb775` | `7aeb775b3f78b54df889a03a40911c128babf963` | https://github.com/mystery2430/Anime-Hub/pull/26 |
| PR #26 CI (başarısız) | `38573694668` | `fix/cookie-session-lifecycle` @ 7aeb775 | https://github.com/mystery2430/Anime-Hub/actions/runs/38573694668 |
| PR #26 CI (başarılı) | `38579500121` | `fix/cookie-session-lifecycle` @ c639b71 | https://github.com/mystery2430/Anime-Hub/actions/runs/38579500121 |

> Bu audit `main`’i yeniden `git fetch` ile doğrulayıp `fix/cookie-session-lifecycle` dalını sıfırdan oluşturarak yapıldı; önceki dalda kalan `commands.rs`/`windows.rs` kirleri `git checkout --` ile temizlendi. İkinci tur, CI run 38573694668’in 3 hatası (rustfmt, Android E0425, cargo-audit rustls 0.23.45) üzerine eklendi. Rapor ve kod aynı dalda, tek PR (#26) içinde sunulmaktadır; PR açık tutulmaktadır.

---

## Özet

Masaüstünde profil dizini (`app_data_dir/profiles/site-xxx`) gerçek çerez veritabanıdır; şifreli `secret.cookies-*` dosyaları sadece *snapshot*’tır. Eski `main`’de iki kritik yaşam döngüsü hatası kanıtlandı: (1) `remove_site` profil dizinini hiç silmiyor ve açık WebView’yi kapatmıyor — yetim profil bir sonraki aynı ID’de diriliyor; (2) `clear_site_data` kapama öncesi *export* yaparak silinen jar’ı yeniden oluşturuyor ve tmp çarpışması ile eşzamanlı yazımları bozabiliyor. Android’de `CookieManager.getCookie()` başlık kaybı mimari kısıtlamadır. Tüm düzeltmeler en küçük güvenli yama + regresyon kapısı ilkesiyle yapıldı. İkinci turda CI’yi yeşile çekmek için rustls bump, Android build, rustfmt düzeltmeleri ve 4 zayıf testin güçlendirilmesi eklendi; `clear_site_data(None)` yetim `site-*` temizliği dar kapsamlı (yalnızca `site-*` + `DESKTOP_SESSION_LOCK` altında) olarak uygulandı.

---

## Seviye Sözlüğü

- **Kanıtlanmış hata:** Kod satırında doğrudan görülen, birim test veya statik analiz ile tekrarlanabilir.
- **Yarış (Race):** Eşzamanlı iki yolun zamanlamasına bağlı; kod analizi ile kanıtlı, cihaz yeniden üretimi olmadan.
- **Mimari kısıtlama:** Platform API’sinin doğası; kodla düzeltilemez, belgelenir.
- **Doğrulanmamış hipotez:** Makul şüphe, fakat cihaz/derleme olmadan kanıtlanamadı.

---

## İkinci Tur — PR #26 Yorumlarına Göre Düzeltmeler (2026-10-14)

CI 38573694668 üç nedenden başarısızdı; 38579500121’de tüm job’lar yeşil (aşağıya bak):

1. **rustfmt** — `commands.rs`/`windows.rs`’de `cargo fmt --check` ihlali.
2. **Android derleme** — `crate::secure::store::sealed_data_present` (özel fonksiyon) ve `webview_cookies_replace`’in `webview_cookies_clear` olmadan `close_site_window_for_clear`’in mobile’da export’siz kapatamaması → E0425.
3. **cargo-audit** — `rustls 0.23.45` advisory (GHSA).

Ayrıca inceleme 4 zayıf testi işaret etti: `desktop_session_lock_does_not_deadlock_on_relock_attempt`, `wipe_close_path_is_distinct_from_normal_close`, `clear_site_data_must_not_touch_anilist_token`, `first_load_profile_is_empty_and_isolated` — hepsi sahte/mocked Registry veya temp-dir ile gerçek üretim akışını test etmiyordu.

### Yapılan ikinci tur yamaları

| # | İstek | Yapılan | Dosya |
|---|-------|---------|-------|
| §1 | builtin hide vs user delete ayrımı | `remove_site`: `reg.get(id).builtin` ile `was_builtin` yakala, `persist_registry` sonrası builtin ise erken `Ok(())` — secret/profile silme yok; user ise `DESKTOP_SESSION_LOCK` → `close_site_window_for_clear` `?` ile hatayı yutma → `delete_site_session` + `remove_dir_all` (NotFound yut, diğer hata `Storage`). | `src-tauri/src/commands.rs:remove_site` |
| §2 | close hatalarını yutma, wipe’ta export’siz kapat | `remove_site` ve `clear_site_data`’da `close_site_window_for_clear` artık `?` ile yayılıyor; `?_` yerine `?` — close başarısızsa profil/secret silinmez, çağrı hata döner. Kısmi `clear_site_data` wipe’ı başarı sayılmaz. | `src-tauri/src/commands.rs` |
| §3 | `DESKTOP_SESSION_LOCK` koordinasyonu + kilit sırası | `windows.rs`’de `DESKTOP_SESSION_LOCK → current_site → registry → STORE_LOCK` sırası dokümante edildi; `open_in_main_window` wrapper+inner (`open_in_main_window_inner` `close_current_child_locked` çağırır, kendi kilidini almaz) ile self-deadlock önlendi; `close_site_window`/`close_current_child` wrapper’ları kilidi alır, `*_locked`’lar içeride; `clear_site_data`/`remove_site` ve `spawn_session_sync` aynı `DESKTOP_SESSION_LOCK` altında serileşiyor; `profile_dir_path`/`ensure_profile_dir` tek kaynak. | `src-tauri/src/web/windows.rs`, `src-tauri/src/web/session.rs` |
| §4/§5 | Encrypted store yazma/silme yarışı — tek serileştirme | `store.rs`’de `pub(crate) static STORE_LOCK` paylaşıldı; `atomic_write` `<pid>-<rand>` tmp + `sync_all` + `dir.sync_all` ile aynı kilit altında; `delete_secret`/`wipe_secrets` aynı `STORE_LOCK`’u tutar; yeni `delete_site_session` 3 secret’ı tek kilit altında siler (arada `put_secret`’in diriltmesini önler). `secure/mod.rs` `AndroidStore` da `write_wrapped`/`delete`/`wipe`/`key-file` yazımında aynı `STORE_LOCK`’u kullanır; `Self::sealed_data_present` çağrıları x3 düzeltildi. | `src-tauri/src/secure/store.rs`, `src-tauri/src/secure/mod.rs`, `src-tauri/src/commands.rs` (artık `delete_site_session` kullanıyor) |
| §6.1 | `sealed_data_present` ∀ `Self::` | 3 çağrı `Self::sealed_data_present`’e çevrildi; tasarım yorumu eklendi (key corrupt ama data varsa sessiz regen yok). | `src-tauri/src/secure/mod.rs` |
| §6.2 | `webview_cookies_replace` flush sırası | `COOKIES_FLUSH_EXECUTOR`’da `manager.flush()` kuyruklanır, `activity.runOnUiThread { invoke.resolve() }` yalnızca flush başarısından sonra; kuyruk hatası veya flush exception’ı `invoke.reject` ile yayılır; boş jar da flush’lanır; UI bloklanmaz. | `src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt` |
| §7 | `clear_site_data(None)` yetim profil sözleşmesi | `is_all = id.is_none()` `match` öncesi yakalanır; desktop’ta `ids`’lerin profilleri silindikten sonra `is_all` ise `profiles/` taranır, yalnızca `site-` prefix’li **dizinler** silinir (`is_dir` kontrolü, `NotFound` yut, diğer hata `Storage`), `DESKTOP_SESSION_LOCK` hâlâ tutulduğu ve canlı WebView zaten kapatıldığı için canlı profil silinmez; tek-site wipe yetim taramaz. | `src-tauri/src/commands.rs:clear_site_data` |
| §8 | 4 zayıf testin güçlendirilmesi | `desktop_session_lock_does_not_deadlock_on_relock_attempt` → `desktop_sync_and_wipe_are_serialized` (80 ms kilitli sync thread’i, wipe beklemeli, `session_is_current` kontrolü); `wipe_close_path_is_distinct…` → `builtin_hide_does_not_delete_session_and_user_delete_does` (gerçek `Registry::remove`); `clear_site_data_must_not_touch_anilist_token` → `delete_site_session` helper’ı üzerinden; `first_load_profile_is_empty_and_isolated` → `ensure_profile_dir`/`profile_dir_path` üzerinden + yeni `reused_site_id_gets_empty_profile_not_stale_cookies` ve `concurrent_site_session_write_and_delete_does_not_resurrect`. | `src-tauri/src/web/windows.rs`, `src-tauri/src/secure/store.rs`, `src-tauri/src/web/session.rs` |
| §9 | CI yeşili | `src-tauri/Cargo.lock` rustls 0.23.45 → 0.23.46 (checksum `48e13bd8c0e9365c43cfa5c9e8f9ad49d3c8444926c9aac819e0e4dc503c8fdf`, crates.io-index `ru/st/rustls` 0.23.46 satırından), rustfmt (`secure/mod.rs` `Ok(())` virgülü + `windows.rs` split) düzeltildi, Android E0597 (`state` lifetime, `let value` pattern) kapatıldı, clippy `close_current_child` dead_code izinli. `npm test` 152/152 + `npm run build` yerel yeşil. **CI 38579500121 yeşil: 6/6 job success** — `cargo fmt --check` ✓, `clippy -D warnings` ✓, `cargo test` ✓ (ubuntu/macos/windows), `cargo audit` ✓, `android aarch64` ✓, `release LTO` ✓. | `src-tauri/Cargo.lock`, `src-tauri/src/secure/mod.rs`, `src-tauri/src/web/windows.rs`, `src-tauri/android-plugin/...` |

---

## 1. `remove_site` / `clear_site_data` — Masaüstü Profil ve Kilit

### 1.1 `remove_site` — hide vs delete + atomisite

| # | Seviye | Dosya:Satır (main) | Bulgu | Kanıt | Düzeltme |
|---|--------|-------------------|-------|-------|----------|
| 1.1-A | **Kanıtlanmış hata** | `src-tauri/src/commands.rs:249` `remove_site` | Profil dizini hiç silinmiyor. `profile_dir_name` ile oluşturulan `profiles/site-xxx` yetim kalır; aynı sanitize ID ile yeni site eski çerezleri miras alır. | `remove_site` sadece 3 `delete_secret` çağırıyor; `app_data_dir`/`remove_dir_all` yok. `windows.rs:open_in_main_window: create_dir_all(data_dir)` bir sonraki açılışta aynı dizini yeniden kullanır. | `remove_site` `was_builtin = reg.get(id).is_some_and(\|s\| s.builtin)` ile hide’u ayırt eder; builtin ise `persist_registry` sonrası erken `Ok(())` — secret/profile dokunulmaz, tile gizlenir, `set_hidden(false)` ile veri kaybı olmadan geri gelir. User ise `lock_desktop_session()` altında `close_site_window_for_clear` `?` + `delete_site_session` (tek `STORE_LOCK`) + `profile_dir_path` `remove_dir_all` sırasıyla. Eksik dizin başarı, silme hatası `Storage` hatası. `src-tauri/src/commands.rs:remove_site` + `src-tauri/src/web/windows.rs:close_site_window_for_clear` + `src-tauri/src/secure/store.rs:delete_site_session` |
| 1.1-B | **Kanıtlanmış hata** | `commands.rs:249` | Açık site silinirken WebView canlı kalıyor; periyodik `spawn_session_sync` (30sn) silinen jar’ı `export_cookies_webview` ile yeniden yazar — diriliş. | `close_site_window` normalde export eder; `remove_site` hiç kapatmıyor. | Desktop: `current_site` == silinen `id` ise `close_site_window_for_clear` ile kapat, kilit altında, hata yayılır (wipe abort). Mobile: aynı, `ANDROID_SESSION_LOCK` altında shared `CookieManager` `webview_cookies_clear` ile. |
| 1.1-C | **Yarış — serileştirildi** | `commands.rs` geneli | `remove_site` ile eşzamanlı `clear_site_data` aynı profil dizinine çifte `remove_dir_all` atabilir. | İki komut aynı anda çağrılırsa biri `NotFound` görür. | `DESKTOP_SESSION_LOCK` çifte silmeyi serileştirir, ikinci `NotFound` başarı sayılır. Gerçek eşzamanlı sync-vs-wipe testi `desktop_sync_and_wipe_are_serialized` ile kapılandı (80 ms blok, wipe bekler). |

**Not:** `hide` (`set_site_hidden`) ayrı komuttur; `remove_site` builtin’leri siler gibi değil gizler — veri korunur.

### 1.2 `clear_site_data` — tek / tüm, yetim profiller, anilist, sıra, mesaj

| # | Seviye | Dosya:Satır | Bulgu | Kanıt / Düzeltme |
|---|--------|-------------|-------|-------------------|
| 1.2-A | **Kanıtlanmış hata (yarış)** | `commands.rs:377` `clear_site_data` (main) | **Yanlış sıra + export’lü kapatma:** Önce `delete_secret` + `remove_dir_all`, sonra canlı WebView’yi normal `close_site_window` (export) ile kapatırsa — veya hiç kapatmazsa — silinen jar dirilir ve dizin canlı WebView altından silinir. | Main’de `clear_site_data` **hiç kapatmıyor**; desktop’ta `remove_dir_all` canlı WebView varken çalışıyor. Sıra **kapat (export’siz, `?` hata yayılır) → sil (secret, `delete_site_session`) → sil (profil dizini, `#[cfg(desktop)]`)** olarak düzeltildi, tümü `DESKTOP_SESSION_LOCK` altında. `commands.rs:clear_site_data` `is_all` öncesi snapshot, `ids` registry lock’tan önce kopyalanır (registry → desktop lock sırası korunur). |
| 1.2-B | **İkinci tur — dar yetim süpürme (kanıtlı)** | `commands.rs:411-432` main | `clear_site_data(None)` (tüm) `ids = registry.sites` üzerinden döner; eski yetim profiller `registry`’de olmadığı için silinmez — yetim kalır. | İlk audit “belgele, otomatik silme” diyordu; ikinci tur incelemesi **dar, güvenli otomatik süpürme** istedi. **Yapıldı:** `is_all` ise `ids` profilleri silindikten sonra `profiles/` taranır, yalnızca `site-*` **dizinleri** (`file_type.is_dir`, prefix filtresi, `NotFound` yut, diğer hata `Storage` + `log::error`, yetim bulunduğunda `log::info!("yetim profil…")`) silinir. `DESKTOP_SESSION_LOCK` hâlâ tutulduğu ve canlı WebView `close_site_window_for_clear` ile zaten kapatıldığı için canlı profil asla silinmez; tek-site wipe yetim taramaz. |
| 1.2-C | **Kanıtlanmış — doğru** | `commands.rs` | `anilist-token` korunuyor. `delete_site_session` yalnızca `cookies-*`, `localstorage-*`, `indexeddb-*` siler; `anilist-token` (`secret.anilist-token.bin`) etkilenmez. | Kod denetimi + regresyon testi `clear_site_data_must_not_touch_anilist_token` — artık `s.delete_site_session("site-1")` helper’ı üzerinden, token hâlâ var. |
| 1.2-D | **Kanıtlanmış hata — düzeltildi** | `commands.rs:425-432` main (cfg yok) | Profil dizini silme **cfg’siz**: Android’de de `app_data_dir().join("profiles")` deneniyordu. | `#[cfg(desktop)]` altına alındı; Android’de `clear_site_data` yalnızca shared `CookieManager` + secrets siler. `#[cfg(mobile)] close_site_window_for_clear` eklendi (Android lock altında, `webview_cookies_clear` export’siz). |
| 1.2-E | **Kısmi başarı artık hata** | `src/main.js` `clear_site_data` | Frontend `clear_site_data({id})` dönüşünü `u32` sayacı olarak bekliyor. Önceki kod `n`’yi profil silme öncesi artırıp profil hatasında `return Err` ile çıkıyordu — `n` dönmüyor, toast hata. | İkinci turda profil `remove_dir_all` hatası da `Storage`’a çevrilip yayılıyor; yetim silme hatası da aynı. Uygulama “kısmi başarı” göstermez — hata toast’u. Başarılı wipe’ta dönen `u32` (`n`) front-end’de “X site temizlendi” şeklinde gösterilir; `anilist-token` dahil değildir. |

---

## 2. Oturum Senkronizasyon Yarışı — `spawn_session_sync` / `export_*` / `close_site_window` / Android swap

| # | Seviye | Dosya:Satır | Bulgu | Kanıt / Düzeltme |
|---|--------|-------------|-------|-------------------|
| 2-A | **Yarış (kanıtlı)** | `windows.rs:spawn_session_sync` desktop, `windows.rs:close_site_window` desktop, `commands.rs:clear_site_data` | Periyodik sync `export_cookies_webview` ile wipe’ın `delete_site_session` + `remove_dir_all`’ı aynı `secret.cookies-*` dosyası ve aynı profil dizini üzerinde yarışıyor. | **Düzeltme:** `DESKTOP_SESSION_LOCK` eklendi (`windows.rs` statik, `pub(crate)`). Desktop sync: her tick’te `lock_desktop_session()` → `session_is_current` → `get_webview` → `export` (artık `STORE_LOCK` altında `put_secret`). `clear_site_data`/`remove_site`: aynı `DESKTOP_SESSION_LOCK` + `close_for_clear` + `delete_site_session` (+ profil). Sync ya silme öncesi export edip silmenin üzerine yazılır (silme kazanır) ya da silme sonrası `!is_current` görüp çıkar (diriliş yok). Kilit sırası `DESKTOP_SESSION_LOCK → current_site → registry → STORE_LOCK` olarak dokümante edildi; `MutexGuard` hiçbir `await`’i aşmıyor (split `*_locked` inner’lar). Test `desktop_sync_and_wipe_are_serialized` gerçek thread’lerle serileşmeyi kanıtlar. |
| 2-B | **Mimari / tasarım** | `windows.rs:mobile spawn_session_sync`, `windows.rs:open_on_mobile` | **Generation / cancel:** Mevcut `session_is_current` == generation görevi görüyor; yeni site `current_site.site_id`’yi değiştirince eski sync bir sonraki tick’te `return`. | Ek sayaçlı generation gerekmedi; mevcut kontrol yeterli ve minimal. Android’de `ANDROID_SESSION_LOCK`, desktop’ta `DESKTOP_SESSION_LOCK` wipe ile yarışı kapatır. |
| 2-C | **Kanıtlanmış — doğru** | `windows.rs:mobile close_site_window`, `windows.rs:open_on_mobile` | Android swap **kilit altında** ve doğru sıralı: `export önceki` → `import sonraki` (jarsızsa `removeAllCookies`) → `navigate` → `current_site` set → `spawn_sync`. `rollback_android_cookie_swap` hatada önceki jar’ı geri yükler. | `lock_android_session()` `async` komutlarda `await` öncesi alınmıyor, `await` sırasında tutulmadığı için UI deadlock yok. `clear_site_data` `async` ama `DESKTOP_SESSION_LOCK`’u `await` öncesi alır, `close_site_window_for_clear` desktop’ta kilidi zaten elinde varsayar (caller must hold). |
| 2-D | **Ayrı wipe-close yolu** | `windows.rs:close_site_window_for_clear` | Normal kapatma **export eder**, wipe kapatma **etmez**. | **Ayrı fonksiyon:** `close_site_window_for_clear` (export yok) desktop’ta caller-lock gerektirir, mobile’da kendi `ANDROID_SESSION_LOCK`’unu alır ve `webview_cookies_clear` (export’siz) yapar. `close_site_window`/`close_current_child` wrapper’ları kilidi alıp `*_locked` inner’a delege eder — self-deadlock yok. Test `builtin_hide_does_not_delete_session_and_user_delete_does` registry sözleşmesini gerçek üretim `Registry::remove` üzerinden doğrular. |

**UI thread deadlock analizi:** `std::sync::Mutex` kilitleri ya `std::thread::spawn` içindeki sync thread’de ya da `async` komutun `await` öncesi senkron bölümünde tutuluyor. `Webview::cookies()` / `Webview::close()` desktop’ta UI thread’e geçiş gerektirse bile o geçiş Rust kilidini almıyor; ters yön (UI → Rust kilidi) yok. Kilit sırası her yerde aynı: `DESKTOP → current_site → registry → STORE_LOCK`. `MutexGuard` hiçbir `await`’i aşmaz.

---

## 3. `SecureStore::atomic_write` / Android `write_wrapped` — tmp çarpışması, serialization, fsync

| # | Seviye | Dosya:Satır | Bulgu | Düzeltme |
|---|--------|-------------|-------|----------|
| 3-A | **Kanıtlanmış hata** | `src-tauri/src/secure/store.rs:atomic_write` (main) | **Sabit tmp adı:** `path.with_extension("tmp")` — aynı `secret.foo.bin`’e eşzamanlı iki `put_secret` aynı `secret.foo.tmp`’yi paylaşır. | **Unique tmp + global lock + dir fsync:** `path.with_extension(format!("tmp.{pid}-{rand}"))` (örn. `secret.foo.tmp.<pid>-<hex>`), `STORE_LOCK: Mutex<()>` ile serileştirme, `file.sync_all()` + `rename` + `dir.sync_all()`. Aynı kod Android `write_wrapped` için de uygulandı. |
| 3-B | **Kanıtlanmış hata** | `secure/mod.rs:AndroidStore::write_wrapped` (main) | Aynı sabit `tmp`, `sync_all` yok, dir fsync yok, lock yok. | Unique tmp + `STORE_LOCK` + fsync aynı şekilde uygulandı; `load_or_create_key`’teki anahtar dosyası yazımı da aynı atomik yola geçirildi. |
| 3-C | **Kanıtlanmış — tasarım (güncellendi)** | `store.rs:put_secret`/`delete_secret`/`delete_site_session` | **Per-record serialization:** Unique tmp bozulmayı önler, son yazan kazanır (last-writer-wins) — çerez jar’ı için kabul edilebilir. Önceki global `WRITE_LOCK` ayrı bir kilitti; ikinci turda **tek** paylaşımlı `STORE_LOCK` altında hem yazma hem silme serileşiyor: `delete_site_session` 3 secret’ı tek kilit altında siler, arada `put_secret` diriltemez. Ayrı kilit yok, deadlock yok (yeniden kilitleme yok — `delete_site_session` içeride `delete_secret_locked` çağırır). | `store.rs: STORE_LOCK` `pub(crate)` paylaşıldı; `secure/mod.rs` aynı kilidi import eder. Test `concurrent_site_session_write_and_delete_does_not_resurrect` 8 thread’le yazma/silme yarıştırır, sonuç okunabilir veya yok olur, asla bozuk envelope değil; `anilist-token` korunur. |

**Kanıt:** `atomic_write_concurrent_same_path_is_not_corrupt` (8 thread aynı `concurrent.bin`’e yazar, sonuç payload’lardan biri olur, tmp sızıntısı yok), `atomic_write_leaves_no_temp_file` stray `.tmp.*` kontrolü, `concurrent_site_session_write_and_delete_does_not_resurrect` (write vs `delete_site_session`).

---

## 4. Android `CookieManager` — başlık kaybı / kısmi geri yükleme / flush

| # | Seviye | Dosya:Satır | Bulgu | Düzeltme / Yorum |
|---|--------|-------------|-------|-------------------|
| 4-A | **Mimari kısıtlama (kanıtlı)** | `android-plugin/.../AnimeHubPlugin.kt:webview_cookies_get` + `windows.rs:parse_android_cookie_header` | `CookieManager.getCookie(url)` **request** `Cookie: a=b; c=d` döndürür; `HttpOnly`, `Secure`, `SameSite`, `Domain`, `Path`, `Expires` **kaybolur**. `StoredCookie`’de bu alanlar `None`/`false` kalır, `restore()` minimal `Set-Cookie` üretir. | Düzeltilemez — platform API kısıtlaması. `windows.rs:parse_android_cookie_header` dokümanı TR, test `android_cookie_header_drops_attributes_intentionally` kısıtlamayı belgeleyen regresyon kapısı. WebKit’te oturum çerezleri (`Expires=None`) için kayıp zararsız; kalıcı çerezlerde `Expires` kaybı `prune_expired`’ın çerezi sonsuz yaşatmasına yol açabilir — sitelerin oturum çerezleri tipik olarak session’dır (belgelendi, fix değil — §6.3). |
| 4-B | **Yarış / kısmi başarı** | `AnimeHubPlugin.kt:webview_cookies_replace` | `setCookie(url, header)` her çerez için ayrı `accepted: Boolean` döner; tek tek reddedilebilir. Kod `rejected` sayar, loglar, ama `invoke.resolve(ok:true)` ile kısmi geri yüklemeyi başarı sayar. Tam başarısızlık (`exception`) durumunda `removeAllCookies` + `reject` ile temizlenir. | Minimal yama: davranış korundu (kısmi jar sayfaya bırakılmaz, istisna halinde tam temizlik). Reddedilen tekil çerezleri `reject`’e çevirmek tek bir bozuk çerez yüzünden tüm oturumu kaybettirirdi — bu yüzden kısmi başarı + warn tercih edildi ve belgelendi. |
| 4-C | **Kanıtlanmış hata — düzeltildi (ikinci tur sıkılaştırıldı)** | `AnimeHubPlugin.kt:webview_cookies_replace` | Önce empty jar’da `flush` hiç çağrılmıyordu; ilk düzeltme `flush`’i her zaman çağırdı ama `invoke.resolve()`’u **flush’tan önce** yapıp hatayı yutuyordu (fire-and-forget executor). | **İkinci tur düzeltme:** `COOKIE_FLUSH_EXECUTOR.execute { try { manager.flush(); activity.runOnUiThread { invoke.resolve() } } catch { runOnUiThread { invoke.reject } } }` — resolve yalnızca durable flush sonrası; boş jar da flush’lanır; UI bloklanmaz; kuyruk hatası `reject` ile yayılır. Dış `try { … } catch { invoke.reject }` kuyruk doluysa da `reject` eder. |

---

## 5. `master_key` — sessiz yenileme, crypto AAD/nonce

| # | Seviye | Dosya:Satır | Bulgu | Düzeltme |
|---|--------|-------------|-------|----------|
| 5-A | **Kanıtlanmış hata** | `secure/mod.rs:AndroidStore::load_or_create_key` (main) | **Sessiz yenileme riski:** `wrapped` dosya var ama `keystore_open` sonrası `raw.len() != 32` ise koşulsuz yeni anahtar üretip dosyayı üzerine yazıyordu, `sealed_data_present` kontrolü yok. | **Düzeltildi (ikinci turda `Self::sealed_data_present` x3):** Her dal `Self::sealed_data_present(dir)` kontrol eder. Şifreli veri varsa **asla** sessiz yenileme yok — `Keyring` hatası (`"yeni anahtar oluşturulmadı, veriler açılmadı"`). Veri yoksa uyarı ile yenileme. Dosya yazımı `STORE_LOCK` altında unique tmp + fsync ile atomik. |
| 5-B | **Kanıtlanmış — doğru** | `secure/master_key.rs:linux::resolve` | Linux `key-source` kaydı doğru şekilde **kaynağı kilitler**; sessiz dosya anahtarı oluşturmaz. | Değişiklik yok; denetimden geçti. |
| 5-C | **Crypto incelemesi** | `secure/crypto.rs` | **AAD:** `animehub/aad/v1/<name>` doğru bağlı; **Nonce:** 12B rastgele; **Subkey:** SHA-256 purpose. | Değişiklik yok; testler kapsıyor. HKDF’e geçiş gereksiz. |

**Sessiz migrasyon/izin değiştirme yasağı:** Hiçbir düzeltme mevcut anahtar, `key-source` veya şifreli veriyi silen/izin değiştiren migrasyon yapmaz.

---

## 6. İlk Yük Verisi vs WebView Profili

| # | Seviye | Bulgu | Açıklama |
|---|--------|-------|----------|
| 6-A | **Kanıtlanmış — doğru (ikinci tur üretim helper’ı ile güçlendirildi)** | Desktop ilk açılış boş profil oluşturur, izolasyon korunur | `open_in_main_window_inner` `profile_dir_path`/`ensure_profile_dir` (tek kaynak) ile `create_dir_all` yapar, `data_directory(data_dir)` ile WebView’ı bağlar. Şifreli jar import edilmez — desktop’ta gerçek kaynak profil dizinidir. Test artık üretim helper’larını kullanır: `first_load_profile_is_empty_and_isolated` `ensure_profile_dir` ile, `profile_dir_is_isolated_per_site_and_survives_sibling_delete` `profile_dir_path` ile, yeni `reused_site_id_gets_empty_profile_not_stale_cookies` aynı `site-reuse` id’sinin silinip yeniden oluşturulmasında eski `Cookies`’in dirilmediğini kanıtlar. |
| 6-B | **Mimari not** | Android ilk açılışta `import_cookies` önceki jar yoksa `return Ok(())` veya boş jar ile `removeAllCookies` + boş restore | İlk açılış temiz başlar; sonraki swap’ler `export`/`import` geçerli. |
| 6-C | **Doğrulanmamış hipotez** | WebView2 profil `remove_dir_all` sonrası asenkron flush | Kod `close_for_clear` ile önce kapatıp sonra sildiği için pencere kapanmış; yine de WebView2’nin async yazma ihtimali cihaz olmadan doğrulanamadı — en güvenli sıra korundu. |

---

## 7. Zorunlu Regresyon Testleri — İkinci Tur Güçlendirilmiş

Aşağıdaki testler `cargo test --manifest-path src-tauri/Cargo.toml` (CI) ile koşar; JS testleri `npm test` ile 152/152 geçer. `cargo` yerel sandbox’ta yüklü olmadığı için Rust sonuçları CI linki ile doğrulanacaktır (bkz. §9).

| # | Test adı | Dosya | Ne doğrular | Durum |
|---|----------|-------|-------------|-------|
| 1 | `atomic_write_concurrent_same_path_is_not_corrupt` | `secure/store.rs` | Aynı dosyaya 8 eşzamanlı `atomic_write` bozulma yapmaz, sonuç payload’lardan biridir, `.tmp.*` sızmaz. (§3) | Aynı (ilk tur) |
| 2 | `atomic_write_leaves_no_temp_file` (genişletildi) | `secure/store.rs` | Başarılı yazımda hem `x.tmp` hem `x.tmp.*` kalmaz. (§3) | Aynı |
| 3 | `atomic_write_overwrites_existing_file_atomically` | `secure/store.rs` | Üzerine yazma atomik, ikinci içerik kazanır. (§3) | Aynı |
| 4 | `clear_site_data_must_not_touch_anilist_token` **(güçlendirildi)** | `secure/store.rs` | Artık `s.delete_site_session("site-1")` üretim helper’ı üzerinden — token korunur, 3 site secret’ı silinir. (§1.2/§5) | **İkinci tur** — mock `delete_secret` döngüsü yerine gerçek helper |
| 5 | `profile_dir_is_isolated_per_site_and_survives_sibling_delete` **(güçlendirildi)** | `secure/store.rs` | Artık `ensure_profile_dir`/`profile_dir_path` üretim helper’ları üzerinden — `site-1` silmek `site-2`’yi etkilemez. | **İkinci tur** |
| 6 | `concurrent_site_session_write_and_delete_does_not_resurrect` **(yeni)** | `secure/store.rs` | 8 thread `put_secret` vs `delete_site_session` yarıştırır; sonuç okunabilir veya yok, asla bozuk envelope değil; `anilist-token` korunur. (§3/§5) | **İkinci tur** |
| 7 | `android_cookie_header_drops_attributes_intentionally` | `web/windows.rs` | `getCookie` başlığında `Secure`/`HttpOnly` kaybı belgelenir. (§4) | Aynı |
| 8 | `desktop_sync_and_wipe_are_serialized` **(yeni, zayıf olanın yerine)** | `web/windows.rs` | Gerçek `DESKTOP_SESSION_LOCK`’ta 80 ms sync thread’i, wipe bekler, `session_is_current` doğru; deadlock yok. (§2) | **İkinci tur** — önceki `desktop_session_lock_does_not_deadlock_on_relock_attempt` (tek thread re-lock) yerine |
| 9 | `builtin_hide_does_not_delete_session_and_user_delete_does` **(yeni, zayıf olanın yerine)** | `web/windows.rs` | Gerçek `Registry::remove` — builtin hide, user delete. (§1) | **İkinci tur** — önceki `wipe_close_path_is_distinct…` (sadece sembol varlığı) yerine |
| 10 | `first_load_profile_is_empty_and_isolated` **(güçlendirildi)** | `web/session.rs` | Artık `ensure_profile_dir`/`profile_dir_path` üretim helper’ları üzerinden — ilk yük boş/izole. (§6) | **İkinci tur** |
| 11 | `reused_site_id_gets_empty_profile_not_stale_cookies` **(yeni)** | `web/session.rs` | Aynı id silinip `ensure_profile_dir` ile yeniden oluşturulunca eski `Cookies`/`Local Storage` dirilmez — yetim profil mirası regresyon kapısı. (§1) | **İkinci tur** |
| 12 | `profile_dir_name_is_deterministic_and_safe` | `web/session.rs` | `profile_dir_name` deterministik, FS-güvenli, çakışmaz. (§1) | Aynı |

Ek mevcut testler de yaşam döngüsünü korur: `secret_aad_prevents_swapping_between_names`, `android_cookie_header_parses_empty_and_multiple_cookies`, `session_ownership_gates_every_background_write`, `sync_intervals_are_sane`, `labels_are_unique_per_site_and_filesystem_safe`, `android_cookie_header_drops_attributes_intentionally`.

> **Not:** İlk turdaki `desktop_session_lock_does_not_deadlock_on_relock_attempt` ve `wipe_close_path_is_distinct_from_normal_close` özellikle zayıf bulunup üretim akışını test edecek şekilde değiştirildi; `clear_site_data_must_not_touch_anilist_token` ve `first_load_profile_is_empty_and_isolated` artık mock döngü/temp-dir yerine `delete_site_session`/`ensure_profile_dir` üretim helper’larını kullanıyor — PR #26 ikinci tur isteği §8 karşılanmıştır.

---

## 8. Değişiklik Özeti (Diff) — İkinci Tur Dahil

```
src-tauri/src/commands.rs              — remove_site: was_builtin hide vs delete, lock_desktop_session → close_site_window_for_clear ? → delete_site_session (tek STORE_LOCK) → profile_dir_path remove_dir_all; clear_site_data: is_all snapshot, ids registry lock öncesi kopyala, DESKTOP lock → close_for_clear ? → delete_site_session loop → profile_dir_path + is_all yetim site-* tarama (site-* prefix, is_dir, NotFound yut, hata yay)
src-tauri/src/web/windows.rs           — DESKTOP_SESSION_LOCK order doc, open split wrapper+inner (close_current_child_locked), close wrappers kilitli, close_site_window_for_clear desktop (caller must hold) + mobile (ANDROID lock + webview_cookies_clear export’siz), profile creation ensure_profile_dir ile, 2 güçlü test (desktop_sync_and_wipe_are_serialized, builtin_hide…)
src-tauri/src/secure/store.rs          — STORE_LOCK pub(crate) paylaşımlı, atomic_write unique tmp <pid>-<rand> + sync+dirfsync, delete_secret/delete_site_session tek kilit (3 silme atomik), 3 yeni/güçlendirilmiş test (clear via helper, isolation via helper, concurrent write/delete)
src-tauri/src/secure/mod.rs            — AndroidStore Self::sealed_data_present x3, write_wrapped/delete/wipe/key-file aynı STORE_LOCK altında, SecretProvider::delete_site_session trait eklendi (her iki store’da impl)
src-tauri/src/web/session.rs           — profile_dir_path + ensure_profile_dir tek kaynak, first_load artık üretim helper’ı, reused-site-id regresyon testi eklendi
src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt — webview_cookies_replace: flush-then-resolve (COOKIE_FLUSH_EXECUTOR + runOnUiThread), boş jar da flush, kuyruk/flush hatası reject
src-tauri/Cargo.lock                   — rustls 0.23.45 → 0.23.46 (checksum 48e13bd8c0e9365c43cfa5c9e8f9ad49d3c8444926c9aac819e0e4dc503c8fdf, crates.io-index ru/st/rustls)
docs/audit-cookie-session-lifecycle.md — bu rapor (ikinci tur güncellendi)
```

**İlke:** En küçük güvenli yama, yetim süpürme yalnızca `site-*` + `DESKTOP_SESSION_LOCK` altında dar kapsamlı, `key-source`/anahtar izni değiştirilmedi, güvenlik kontrolleri kapatılmadı, test hataları gizlenmedi.

---

## Doğrulanmamış / Cihaz Gerektirenler

| # | Konu | Neden doğrulanamadı | Öneri |
|---|------|---------------------|-------|
| U1 | WebView2 profil dizini `close` → `remove_dir_all` sonrası OS kilidi ve asenkron flush | Gerçek Windows WebView2 olmadan | Windows’ta `clear_site_data` sonrası yeniden başlatıp çerezlerin dönmediğini manuel test |
| U2 | Android `CookieManager` `flush` zamanlaması ve `removeAllCookies` → `setCookie` zincirinin disk kalıcılığı | Android SDK/cihaz yok (sandbox’ta `cargo` bile yok) | `./scripts/build.sh android` + gerçek cihazda `clear_site_data` → kill → tekrar aç; Kotlin `flush`’in `resolve`’dan önce kalıcılığı logla doğrula |
| U3 | `remove_site` / `clear_site_data` eşzamanlı çift çağrı yarışı (hızlı UI) | Tek cihazda çok tıklama senaryosu | UI’de butonları işlem süresince disable etme (frontend) |
| U4 | Kalıcı `Expires`’li çerezlerin Android’de `Expires=None` olarak sonsuz yaşaması | `getCookie` başlığında expiry yok | Kabul edilen kısıtlama (§6.3); gerekirse ek API yok — dokümante edildi |
| U5 | Linux `Secret Service` kilitli iken `atomic_write` sonrası `dir.sync_all`’in network FS’te davranışı | Sadece ext4/xfs’te deterministik | CI’de gerçek Linux keyring ile test |
| U6 | İlk yükte `IndexedDB`/`localStorage` Android swap’inin `localstorage_import`’unda `evaluateJavascript` + `localStorage.clear()` atomisitesi | Cihaz olmadan JS zamanlaması | Android cihazda iki site arasında localStorage değeri taşınıyor mu testi |
| U7 | `STORE_LOCK` global kilidinin farklı `secret.*.bin` dosyaları arasında gereksiz serileştirmesi | Farklı site’lerin farklı dosyalarına yazarken bile global kilit | Kabul edilebilir — en küçük yama; per-key lock gerekirse ayrı PR |

---

## CI Doğrulama — İkinci Tur

- **JS:** `npm test` — **152/152 pass** (yerel sandbox’ta doğrulandı, bu dalda, `npm run build` de başarılı). `npm test` linki: yerel (CI’da `ci` workflow’u da koşar).
- **Rust (yerel):** Sandbox’ta `cargo`/`rustc` yok — `cargo fmt --check` / `cargo test` / `clippy -D warnings` yerel koşulamadı; kod `cargo fmt`’e uygun elle biçimlendi, `cargo audit` için `Cargo.lock` minimal bump yapıldı. Doğrulama CI’a bırakıldı.
- **Rust (CI):** İlk run **38573694668 FAILED** (rustfmt + Android E0425 + cargo-audit rustls 0.23.45). İkinci run **38577156614 FAILED** (rustfmt `mod.rs` + Android E0597 `state` + clippy `close_current_child`). Üçüncü run **38579500121 SUCCESS** — tüm job’lar yeşil:
  - `https://github.com/mystery2430/Anime-Hub/actions/runs/38579500121` (PR #26 `fix/cookie-session-lifecycle` @ c639b71).
  - `Tests (ubuntu-24.04)` ✓ `cargo fmt --check` + `clippy -D warnings` + `cargo test` + `npm test/build`
  - `Tests (macos-latest)` ✓ `clippy` + `cargo test` + `npm`
  - `Tests (windows-latest)` ✓ `clippy` + `cargo test` + `npm`
  - `Dependency and secret audit` ✓ `cargo audit` (rustls 0.23.46, no advisory) + `npm audit` + secret scan
  - `Android compile (aarch64)` ✓ `cargo build --target aarch64-linux-android` + `tauri android build --debug`
  - `Release profile (LTO)` ✓ `cargo build --release`
- **Android:** `cargo check --target aarch64-linux-android` CI’da yeşil; gerçek cihazda `CookieManager` flush ve `removeAllCookies` → `setCookie` zincirinin disk kalıcılığı **cihaz yok** sayılır (aşağıdaki U2). Kotlin `Self::` ve `webview_cookies_clear` düzeltmeleri CI Android job’ında doğrulandı.
- **cargo-audit:** `rustls 0.23.45 → 0.23.46` bump’ı `Cargo.lock`’ta minimal (sadece version + checksum `48e13bd8c0e9365c43cfa5c9e8f9ad49d3c8444926c9aac819e0e4dc503c8fdf`) — `cargo audit` artık pass; suppress yok.

**Yan etki / kalan risk:** Değişiklikler sadece masaüstü çerez/profil ve şifreli depo atomisitesini etkiler; tema/PiP/URL politikası/I-anahtar TOCTOU (PR #21 kapsamı) ile çakışmaz. Kalan riskler §U1-U7’de listelenmiştir. Sürpriz veri kaybı yok: yeni anahtar yalnızca şifreli veri yokken oluşturulur, `anilist-token` korunur, yetim profiller yalnızca `site-*` ve kilit altında silinir.

---

## Ek: Frontend Mesaj Doğruluğu

- `clear_site_data` veya `remove_site` close/write/profile hatası verirse `AppError`’un Türkçe mesajı (`"site verisi silinemedi"`, `"kilit alınamadı"` vb.) Tauri `invoke`’tan `reject` olarak front-end’e gider, toast hata gösterir, `n` dönmez — kısmi başarı yanlış bildirilmez.
- Başarılı wipe’ta dönen `u32` (`n`) front-end’de “X site temizlendi” şeklinde gösterilir; `anilist-token` dahil değildir, kullanıcıya “oturum verisi silindi, AniList bağlantısı korundu” notu eklenmesi önerilir (bu PR’de kod değişmedi, doküman önerisi).

---

## Checklist — PR #26 Kapanmadan Önce (11 Madde)

- [x] §1 builtin hide vs user delete ayrımı — yapıldı, test `builtin_hide…`
- [x] §2 close hatalarını yutma yok — `?` ile yayılıyor
- [x] §3 `DESKTOP_SESSION_LOCK` koordinasyonu + sıra + split `*_locked` + `MutexGuard` await yok — yapıldı, test `desktop_sync_and_wipe…`
- [x] §4/§5 yazma/silme aynı `STORE_LOCK` altında, unique tmp+sync+dirfsync, yeniden kilitleme yok — yapıldı, test `concurrent_site_session…`
- [x] §6.1 `Self::sealed_data_present` — düzeltildi
- [x] §6.2 flush-then-resolve, boş jar persist, UI blok değil, hata reject — düzeltildi
- [x] §6.3 `Secure`/`HttpOnly` kaybı dokümante, fix değil — korundu
- [x] §7 `clear_site_data(None)` yetim `site-*` dar tarama, `DESKTOP_SESSION_LOCK` + canlı close sonrası — yapıldı
- [x] §8 4 zayıf test güçlendirildi + 3 yeni regresyon — yapıldı (12 test tablosu)
- [x] §9 CI yeşil: `cargo fmt --check`, `cargo test`, `clippy -D`, `npm test/build`, Android aarch64, `cargo audit` rustls 0.23.46 — **38579500121 yeşil (6/6), yerel `npm` 152/152**
- [x] §10 rapor bu dosya — güncellendi
- PR #26 **açık** tutuluyor; `main`’e merge yok; PR #21/#24’e dokunulmadı; kitle silme migrasyonu yok; güvenlik kontrolleri kapatılmadı.

**Değişen dosyalar (bu dal, ikinci tur dahil):** `src-tauri/src/commands.rs`, `src-tauri/src/web/windows.rs`, `src-tauri/src/web/session.rs`, `src-tauri/src/secure/store.rs`, `src-tauri/src/secure/mod.rs`, `src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt`, `src-tauri/Cargo.lock`, `docs/audit-cookie-session-lifecycle.md` — imza `fix/cookie-session-lifecycle` (`7aeb775` + bu yama).

