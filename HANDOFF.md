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

**Bilinen sınır (dürüstçe):** bu bir **metin** kontrolü. Herkese açık görünen
ama özel bir adrese çözümlenen bir alan adı (DNS rebinding) hâlâ geçer; guard'ı
eşzamanlı tutmak için DNS çözümlemesi yapılmıyor. README'de de yazıyor.

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
| **Android derlemesi** | Android SDK/NDK kurulu değil. `tauri android build` hiç çalıştırılamadı |
| **Kotlin köprüsü** | `android-plugin/kotlin/dev_animehub_app/AnimeHubPlugin.kt` Tauri 2.11.6 API'sine göre yazıldı ama **derlenmedi** |
| **PiP gerçek cihazda** | Sistem PiP API'si emülatörde bile davranış farklılığı gösterir; cihaz yok |
| **Windows derlemesi** | NSIS yalnızca Windows'ta; bu ortam Linux |
| **GitHub Actions** | Üç workflow yazıldı ama hiçbir runner'da çalışmadı |
| **Ayarlanmış profille paket** | 1984 MB RAM'de `lto = true` OOM veriyor |
| **AniList canlı OAuth** | Geçerli `client_id`/`client_secret` yok; akış birim testleriyle doğrulandı, gerçek sunucuya karşı değil |
| **Gerçek sitelerin yüklenmesi** | Uygulama GUI'si başsız ortamda açılmıyor; WebView'da `openani.me`'nin gerçekten render olduğu görülmedi |

---

## 6. Claude'tan istenenler — öncelik sırasıyla

### A. Android'i gerçekten derlemek (en kritik boşluk)

```bash
# SDK + NDK kur, sonra:
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
export ANDROID_HOME=…  NDK_HOME=$ANDROID_HOME/ndk/<sürüm>
npm run tauri android init
```

`tauri android init` sonrası **iki el işi** gerekiyor:

1. `src-tauri/android-plugin/kotlin/dev_animehub_app/AnimeHubPlugin.kt`
   dosyasını `src-tauri/gen/android/app/src/main/java/dev_animehub_app/`
   altına kopyala. **Üretilen paket adını kontrol et** — Rust tarafındaki
   `register_android_plugin("dev_animehub_app", "AnimeHubPlugin")`
   çağrısıyla birebir aynı olmalı.
2. `AndroidManifest.xml`'e `android:supportsPictureInPicture="true"` ekle
   (ayrıntı: `src-tauri/android-plugin/AndroidManifest.notes.md`).

Bilinen risk: Kotlin dosyasındaki `KeyGenParameterSpec` /
`PictureInPictureParams` kullanımları derleyici görmedi. İlk derlemede imza
uyuşmazlığı çıkarsa şaşırmayın.

### B. Ayarlanmış profille Linux paketi

7 GB+ bellekli bir makinede:

```bash
npm run tauri build -- --bundles appimage deb rpm
```

`lto = true` + `codegen-units = 1` ile derlenip derlenmediğini doğrula.
Burada OOM verdiği için **hiç denenemedi**.

### C. GitHub Actions'ı bir kez çalıştırmak

`ci.yml`, `release-android.yml`, `release-desktop.yml`. Özellikle
`release-android.yml`'deki keystore akışı (`secrets.ANDROID_KEYSTORE_BASE64` →
geçici dosya → `shred -u`) hiç test edilmedi.

### D. DNS rebinding

`decide_navigation` eşzamanlı kalmak zorunda. Çözüm seçenekleri: (1) navigasyon
öncesi async çözümleme, (2) WebView'ın kendi bağlantı katmanında filtre,
(3) kabul edip belgelemek (şu anki durum). Karar verilmedi.

### E. Depoyu yayınlarken

README'deki `github.com/animehub/animehub` **yer tutucu**. Gerçek
`kullanıcı/depo` ile değiştirilmeli (3 rozet + 2 bağlantı).

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
cd src-tauri && cargo test              # 148 test
cd .. && node --test tests/             # 45 test
npm run check                           # ikisi birden
./scripts/build.sh linux                # paketle
```

Kilit dosyalar:

| Yol | Ne |
|---|---|
| `src-tauri/src/sites/url_policy.rs` | HTTPS + özel adres politikası (`is_private_host` burada) |
| `src-tauri/src/web/session.rs` | gezinti kararı, `embed_json`, çerez yakalama |
| `src-tauri/src/web/windows.rs` | `on_navigation` / `on_new_window` guard'ları |
| `src-tauri/src/secure/` | AES-256-GCM depo + platform anahtar kaynağı |
| `src-tauri/tests/audit.rs` | denetim + regresyon testleri |
| `tests/security.test.js` | derlenmiş çıktı üzerinde XSS/secret taraması |
| `NOTICES.md` | temiz oda beyanı ve atıflar |
