# Devir Notu — AnimeHub

Bu dosya, projeyi devralan geliştiriciye (veya Claude'a) yöneliktir. Amaç:
**nelerin gerçekten doğrulandığını, nelerin doğrulanmadığını ve neden
doğrulanamadığını** tek yerden vermek. Tahmin yok; her satır ya çalıştırılmış
bir komutun çıktısı ya da açıkça "yapılamadı" olarak işaretli.

---

## 1. Proje bir cümlede

Tauri 2 (Rust + native WebView) ile yazılmış bir **başlatıcı**: ikon ızgarası,
seçilen site kendi izole ve şifreli WebView oturumunda tam ekran açılır.
Android birincil, Windows/Linux ikincil hedef; macOS/iOS kapsam dışı.

Paket kimliği `dev.animehub.app`. GPLv3 lisanslı Dantotsu'dan **hiç kod
taşınmadı** (temiz oda); ayrıntı `NOTICES.md` içinde.

---

## 2. Doğrulandı — komut ve sonuç

Geliştirme ortamı: Linux x86_64, Debian 13, **1984 MB RAM**, Node v20.20.2,
rustup stable (1.98.x).

| Komut | Sonuç |
|---|---|
| `cargo test` (src-tauri) | **137 birim + 11 denetim = 148 geçti**, 0 hata |
| `cargo clippy --all-targets -- -D warnings` | temiz, uyarı yok |
| `cargo fmt --all -- --check` | temiz |
| `node --test tests/` | **45 geçti**, 0 hata |
| `npm run build` | başarılı (vite) |
| `npm run tauri build -- --bundles deb` | `AnimeHub_0.1.0_amd64.deb` (3.7 MB) üretildi |

Üretilen `.deb` içeriği doğrulandı: `usr/bin/animehub` (9.958.688 bayt),
ikonlar, ve `MimeType=x-scheme-handler/animehub` içeren `.desktop` dosyası.

> Not: `.deb`, `TAURI_LOW_MEMORY=1` ile (`lto = false`, `codegen-units = 16`)
> üretildi. Depodaki ayarlanmış profil (`lto = true`) bu makinede `gtk`
> derlenirken **OOM ile öldürüldü**. 7 GB bellekli bir runner'da ayarlanmış
> profilin çalışması beklenir ama **burada denenemedi**.

---

## 3. Bu turda bulunan ve düzeltilen güvenlik açığı

**Açık:** `web::session::decide_navigation` yalnızca URL şemasına bakıyordu.
`validate_site_url` kaydedilen site adresini özel/yerel adreslere karşı
koruyordu, **ama bir sitenin WebView'ı sonradan yönlendirmesi bu kontrolden
geçmiyordu.** Yani güvenilen bir site (veya ele geçirilmiş bir reklam
ağı) WebView'ı şunlara yönlendirebilirdi:

- `https://192.168.1.1/` — kullanıcının yönlendiricisi
- `https://169.254.169.254/latest/meta-data/` — bulut metadata ucu
- `https://127.0.0.1/…` — `tauri.localhost` dahil yerel yüzeyler

**Düzeltme:** `is_private_host` artık `pub`; `decide_navigation` blok listesi
kontrolünden önce onu çağırıyor ve `NavDecision::Block` dönüyor.

**Regresyon testi:** `src-tauri/tests/audit.rs::navigation_guard_blocks_private_hosts`
(7 vektör) ve `navigation_guard_allows_public_https` (yanlış pozitif koruması).

**Bilinen sınır (dürüstçe, güncellendi):** metin kontrolü duruyor. Üstüne,
gezinti ve site açılışı artık alan adını kısa bir zaman aşımıyla çözüyor
(`web/dns.rs`); cevaplardan herhangi biri özel/yerel ise kesiliyor. Çözümleyici
zaman aşımına uğrarsa metin politikası geçerli kalır (DNS kesintisi siteleri
kapatmasın diye). Sayfa yüklendikten sonra WebView'ın kendi çözümleyicisiyle
giden alt istekler (`fetch` / XHR) hâlâ bu kontrolden geçmez — onu kapatmak
filtreleyen bir vekil ister, v1'de yok. Karar `HANDOFF` bölüm 6.D'de.

---

## 4. Denetimde test edilip **sağlam çıkan** şeyler

`tests/audit.rs` bunları kalıcı olarak kilitler:

- **Varsayılan siteler çalışıyor.** `Registry::new_default()` tam olarak
  `builtin-openanime` → `https://openani.me/` ve `builtin-animecix` →
  `https://animecix.com/` veriyor; ikisi de `Watching`, `builtin: true`,
  görünür. İkisi de kendi URL politikasını geçiyor ve kendi gezinti
  guard'ından `Allow` alıyor.
- **Reddedilen 24 vektör:** `http`, `javascript:`, `data:`, `file:`, `blob:`,
  `ftp:`; `127.0.0.1`, ondalık `2130706433`, onaltılık `0x7f.1`, `localhost`,
  `[::1]`, `[::ffff:127.0.0.1]`, `192.168.1.1`, `10.0.0.1`, `172.16.0.1`,
  `169.254.169.254`, `0.0.0.0`, `*.local`, `*.internal`, `tauri.localhost`;
  gömülü kullanıcı adı/parola.
- **`embed_json` kaçışı yok.** `</script>`, `</SCRIPT >`, ham U+2028/U+2029
  hepsi kaçıyor ve çıktı geçerli JSON kalıyor.
- **`profile_dir_name` yol kaçışı yok.** `../../etc/passwd` → `site-etc`,
  NUL ve `/` temizleniyor, aynı girdi aynı adı veriyor.
- **Çerez bileşenlerinde CRLF enjeksiyonu yok.**
- **Frontend'de `innerHTML`/`outerHTML`/`insertAdjacentHTML`/`document.write`
  yok** — hem `src/` hem **derlenmiş `dist/assets/*.js`** üzerinde zorlanıyor
  (`tests/security.test.js`).
- **`tauri.conf.json`:** `assetProtocol.enable === false`, CSP
  `script-src 'self'` + `frame-src 'none'` + `object-src 'none'`, wildcard
  yok, `unsafe-eval` yok.
- **`capabilities/default.json`:** yalnızca `["main"]` penceresine kapsamlı,
  `shell:allow-execute` yok, `opener:allow-open-url` yalnızca `https://**`.
- **Depoda gizli anahtar yok** (deset taraması + keystore izleme kontrolü).

---

## 5. Doğrulanmadı — ve neden

Bunlar **eksik**, "çalışıyor" diye sunulmamalı:

| Konu | Neden yapılamadı |
|---|---|
| **Android APK** | Init ve prepare runner'da geçti. APK adımı henüz yeşil değil; cihaz testi yok |
| **Kotlin köprüsü** | `android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt`. Paket yolu doğrulandı; Gradle derlemesi henüz yeşil değil |
| **PiP gerçek cihazda** | Sistem PiP API'si emülatörde bile davranış farklılığı gösterir; cihaz yok |
| **Windows NSIS** | CI Windows'ta test eder, paket üretmez |
| **İmzalı yayın APK** | `release-android.yml` varsayılan dalda değil; dispatch/etiket hiç çalışmadı |
| **`.deb` / AppImage / rpm** | LTO'lu `cargo build --release --locked` ubuntu-24.04'te geçti. Paket adımı yok |
| **AniList canlı OAuth** | Geçerli `client_id`/`client_secret` yok; akış birim testleriyle doğrulandı, gerçek sunucuya karşı değil |
| **Gerçek sitelerin yüklenmesi** | Uygulama GUI'si başsız ortamda açılmıyor; WebView'da `openani.me`'nin gerçekten render olduğu görülmedi |

---

## 6. Claude'tan istenenler — öncelik sırasıyla

### A. Android'i gerçekten derlemek (en kritik boşluk)

El işi kopyalama kalktı. `tauri android init` sonrası:

```bash
python3 scripts/android_prepare.py
```

Script üç dizeyi karşılaştırır ve uyuşmazsa durur: Kotlin `package` satırı,
`register_android_plugin(...)` ve üretilen `MainActivity.kt` paketi. Tauri
kimliği `dev.animehub.app` noktalı paket olarak üretir; eski
`dev_animehub_app` tahmini yanlıştı ve düzeltildi. Script ayrıca PiP
özniteliğini, eksikse OAuth intent-filter'ını ve R8 keep kuralını yazar.
`./scripts/build.sh android` ve `release-android.yml` bunu kendisi çağırır.

`release-android.yml` içindeki iki gerçek hata da düzeltildi: keystore
dosyası `gen/android` oluşmadan yazılıyordu, ve `if:` içinde `secrets`
kullanılıyordu (Actions bunu reddeder).

Bilinen risk: Kotlin, Tauri 2 `JSObject.getInteger` / `getBoolean` /
`put` imzalarına göre yazıldı (kaynakla karşılaştırıldı). Runner'da init ve
prepare geçti; APK adımı henüz yeşil değil.

### B. Ayarlanmış profille Linux paketi

7 GB+ bellekli bir makinede:

```bash
npm run tauri build -- --bundles appimage deb rpm
```

`lto = true` + `codegen-units = 1` ile `cargo build --release --locked`
ubuntu-24.04 runner'da geçti (OOM yok). `.deb` / AppImage / rpm hâlâ üretilmedi.

### C. GitHub Actions'ı bir kez çalıştırmak

`ci.yml` PR #1'de çalıştı: frontend testleri üç platformda, rustfmt, ubuntu
clippy/test ve LTO linki geçti. `release-android.yml` varsayılan dalda
olmadığı için dispatch/etiket hiç çalışmadı; keystore akışı
(`secrets.ANDROID_KEYSTORE_BASE64` → geçici dosya → `shred -u`) test edilmedi.

### D. DNS rebinding — karar verildi

Seçenek (1), eşzamanlı tutularak: `on_navigation` ve `open_site` alan adını
1200 ms zaman aşımıyla çözer; herhangi bir cevap özel/yerelse keser.
Zaman aşımı / NXDOMAIN seçenek (3) gibi metin politikasına düşer — aksi halde
DNS kesintisi bütün siteleri kapatır. Seçenek (2) (bağlantı katmanında vekil)
yapılmadı: WebView alt isteklerini kendi çözer, vekil olmadan TOCTOU kapanmaz,
ve vekil burada doğrulanamazdı. Kalan sınır README'de ve bölüm 3'te yazılı.

### E. Depoyu yayınlarken — yapıldı

README'deki `github.com/animehub/animehub` yer tutucusu
`mystery2430/Anime-Hub` ile değiştirildi (rozetler, klon adresi, Issues).

---

## 7. Ortam notu — sandbox sıfırlanıyor

Bu çalışma alanında **oturumlar arasında araç zinciri kayboluyor**:
`~/.cargo`, `~/.rustup`, `node_modules` ve `src-tauri/target` siliniyor,
`apt` ile kurulan sistem kütüphaneleri de gidiyor. Yalnızca proje kaynak
dosyaları kalıcı; derleme çıktıları ve bağımlılıklar her oturumda yeniden
kurulmalı. (Bu, çalışma alanının dışlama listesinden kaynaklanıyor —
128 MB / 10.000 dosya sınırından değil; kalıcı kısım 1.4 MB / 108 dosya.)

Yeniden kurmak için:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
rustup component add rustfmt clippy
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev pkg-config build-essential
cd animehub && npm install
```

---

## 8. Hızlı başvuru

```bash
cd src-tauri && cargo test              # birim + denetim
npm test                                # frontend (dosya listesi; `tests/` dizini Node 22'de kırılıyor)
npm run check                           # ikisi birden
./scripts/build.sh linux                # paketle
python3 scripts/android_prepare.py      # `tauri android init` sonrası
```

Kilit dosyalar:

| Yol | Ne |
|---|---|
| `src-tauri/src/sites/url_policy.rs` | HTTPS + özel adres politikası (`is_private_host`, `is_public_ip`) |
| `src-tauri/src/web/dns.rs` | eşzamanlı DNS rebinding kontrolü |
| `src-tauri/src/web/session.rs` | gezinti kararı, `embed_json`, çerez yakalama |
| `scripts/android_prepare.py` | init sonrası Kotlin kopyası + PiP manifest |
| `src-tauri/src/web/windows.rs` | `on_navigation` / `on_new_window` guard'ları |
| `src-tauri/src/secure/` | AES-256-GCM depo + platform anahtar kaynağı |
| `src-tauri/tests/audit.rs` | denetim + regresyon testleri |
| `tests/security.test.js` | derlenmiş çıktı üzerinde XSS/secret taraması |
| `NOTICES.md` | temiz oda beyanı ve atıflar |
