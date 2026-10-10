# Devir Notu — AnimeHub

Bu dosya, projeyi devralan geliştiriciye (veya Claude'a) yöneliktir. Amaç:
**nelerin gerçekten doğrulandığını, nelerin doğrulanmadığını ve neden
doğrulanamadığını** tek yerden vermek. Tahmin yok; her satır ya çalıştırılmış
bir komutun çıktısı ya da açıkça "yapılamadı" olarak işaretli.

---

## v0.3.3 yayın devri — 2026-10-10

- Güncel sürüm: **0.3.3** (npm, Tauri, iki Rust paketi ve lockfile tutarlı).
- Yerel: `npm test` **152 geçti / 0 hata**; `npm run build` başarılı;
  `npm audit` **0 açık**. Rust/Android araç zinciri yok; bu kontroller CI’da yapılır.
- PR CI, Release Desktop ve Release Android aynı yayın ağacı için geçmeden
  tag/release oluşturulmaz. Release workflow’ları yalnızca paket üretir;
  yayın prosedürü ve kanıt kaydı: [`docs/releasing.md`](docs/releasing.md).
- **v0.3.3 fiziksel Android testi yapılmadı.** PiP/IPC/gezinti/performans
  doğrulaması ve gerçek Linux Secret Service denemesi bekliyor.
- **Android sağlayıcıları arasında aynı-origin localStorage/IndexedDB
  izolasyonu garanti edilmez.** Aktarım güvenlik sınırı değildir, kayıpsız değildir.
- CI debug APK’sı GitHub Release’e eklenmez. Yalnızca imzası doğrulanmış
  üç ABI release APK’sı ve dört masaüstü paketi yayımlanır.
- v0.3.2 tag’i ve release’i tarihsel kayıt olarak korunur.

Aşağıdaki eski oturum kayıtları tarihsel kanıttır; v0.3.3 cihaz testi sonucu değildir.

---

## 1. Proje bir cümlede

Tauri 2 (Rust + native WebView) ile yazılmış bir **başlatıcı**: ikon ızgarası,
site masaüstünde kendi WebView profiliyle açılır; Android tek WebView kullanır
ve sağlayıcı çerez kavanozlarını değiştirir. Android'de aynı kaynaklı
`localStorage`/IndexedDB için gerçek sağlayıcı yalıtımı henüz çözülmedi.
Android birincil, Windows/Linux ikincil hedef; macOS/iOS kapsam dışı.

Paket kimliği `dev.animehub.app`. GPLv3 lisanslı Dantotsu'dan **hiç kod
taşınmadı** (temiz oda); ayrıntı `NOTICES.md` içinde.

---

## 2. Doğrulandı — komut ve sonuç

Geliştirme ortamı: Linux x86_64, Debian 13, **1984 MB RAM**, Node v20.20.2 (önceki oturum; 2026-10-10 oturumunda Node v22.22.3 kullanıldı),
rustup stable (1.98.x).

| Komut | Sonuç |
|---|---|
| `cargo test` (src-tauri) | Bu oturumda yerelde **çalıştırılmadı** (sandbox'ta Rust araç zinciri yok). Son yeşil sonuç CI'daki Tests işleri (ubuntu-24.04, macos-latest, windows-latest); eski yerel sayı 137 birim + 11 denetimdi |
| `cargo clippy --all-targets -- -D warnings` | temiz, uyarı yok |
| `cargo fmt --all -- --check` | temiz |
| `npm test` | **118 geçti**, 0 hata (2026-10-10, yerel; çıktı `# pass 118` / `# fail 0`, çıkış kodu 0) |
| `node --test tests/` (dizin argümanı) | Yerelde Node 22.22.3 ile çalışmadı: `Cannot find module '…/tests'`, çıkış kodu 1. Proje `npm test` ile dosya listesi kullanır; dizin argümanı kullanılmamalı |
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
- **Depoda gizli anahtar yok.** İzlenen dosyalarda keystore, `.pem`, `.env` veya token literali yok. CI'daki "possible secret" adımı kendi arama metnine takılıyordu; metin artık dosyada bitişik durmuyor.

---

## 5. Doğrulanmadı — ve neden

Bunlar **eksik**, "çalışıyor" diye sunulmamalı:

| Konu | Neden yapılamadı |
|---|---|
| **Android APK / v0.3.2 runtime** | Herkese açık v0.3.2 release'i yayımlandı; Android release workflow'u aarch64, armv7 ve x86_64 için imzalı APK'ları başarıyla üretti. Önceki CI APK'sını bir cihazda açarken çökme görülmedi, ancak yükleme yavaştı. Bu sürümdeki asenkron `CookieManager.flush()` değişikliğinin performansı fiziksel cihazda henüz ölçülmedi |
| **Android localStorage/IndexedDB** | Dışa/içe aktarma köprüleri ve şifreli anlık görüntüler bulunur; bunlar gerçek sağlayıcı yalıtımı sağlamaz. Android tek WebView'ı yeniden kullandığı için aynı kaynaklı `localStorage`/IndexedDB izolasyonu çözülmemiştir ve güvenlik sınırı olarak sunulmamalıdır. IndexedDB aktarımı en iyi çabadır (ikincil indeks/key path/Blob kaybı mümkündür) |
| **Kotlin köprüsü** | v0.3.2 release workflow'u Activity/WebView yaşam döngüsü kontrolleri ve asenkron cookie kalıcılaştırması içeren köprüyü üç Android ABI'sinde derleyip imzaladı. Yerelde Android SDK/Gradle yok; asenkron kalıcılaştırmanın gerçek cihaz performansı doğrulanmadı |
| **PiP duraklatma** | 6f28b13'teki 2 saniyelik otomatik oynatma yeniden denemesi kaldırıldı (`c4a260b`). Sayfanın gördüğü bir duraklatma geri alınmaz. Kullanıcı ile WebView duraklatması ayırt edilemediğinden, PiP sırasındaki oynatma sürekliliği **cihazda doğrulanmadı** ve bir sınır olarak kalıyor |
| **Android site gezinme koruması** | `SiteNavigationGuard` (`470e7b8`) yalnızca üst düzey gezinmeleri denetler: düz `http`, https dışı şemalar, yerel/özel hostlar. Blocklist, DNS rebinding, alt çerçeveler ve uygulamanın `loadUrl` çağrıları kapsam dışı. Kotlin derlenmedi, cihazda denenmedi. Ayrıntı: `docs/android-navigation.md` |
| **Android `ipc` kısıtı** | wry'nin her sayfaya açtığı `ipc` nesnesi kaldırıldı; launcher origin'lerine ana çerçevede yalnızca `WebMessageListener` verildi (`a864bad`). Kurulamazsa hata loglanır ve IPC kapalı kalır (fail closed). CI Android derlemesi (`5ef743f`, run 38047701321) geçti. **Cihazda doğrulanmadı**: launcher IPC'si çalışmalı, bir site sayfasında `typeof window.ipc` `"undefined"` olmalı |
| **Linux anahtar kaynağı (keyring)** | Anahtar kaynağı `key-source` kaydında tutulur (`e001705`). Kilitli/erişilemeyen keyring'de yeni anahtar oluşturulmaz, kayıtlı keyring anahtarı varken dosya anahtarına geçilmez. CI `cargo test --all` Linux, Windows ve macOS'ta geçti (`a21b0ec`, run 38045901411). Gerçek Secret Service ile test edilmedi |
| **PiP çalışma zamanı (gerçek cihaz)** | Sistem PiP API'si emülatörde bile davranış farklılığı gösterir; bu ortamda cihaz yok. Kontrollü zincir testlerden geçti ve PR #19'un CI koşusunda (`6f28b13` için run 37985547812, tüm işler yeşil) Android aarch64 **derlendi, debug APK üretildi** (`animehub-android-aarch64-debug`). Bu tarihten sonraki commit'ler için CI sonucu ayrıca kontrol edilmelidir. Yani "derleniyor" doğrulandı, "cihazda çalışıyor" **doğrulanmadı**: PiP geçişi, oynatıcı seçimi, geri yükleme ve API seviyesi farkları hâlâ cihazda denenmeli (`docs/android-pip.md` §12'deki 15 maddelik liste) |
| **Windows NSIS** | v0.3.2 Release Desktop workflow'u Windows üzerinde geçti ve `AnimeHub_0.3.2_x64-setup.exe` üretti. Windows imzalama sırrı tanımlı olmadığı için paket imzasız; README'de uyarısı var |
| **macOS** | Hedef değil. `cargo test` `77f5e7c`'de geçti; anahtar Linux yedeğiyle aynı `0600` dosya, Keychain yok |
| **İmzalı yayın APK** | ✅ Keystore secret'ları (`ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`) repository secrets'a eklendi; workflow imzalı üretiyor |
| **`.deb` / AppImage / rpm** | v0.3.2 Release Desktop Ubuntu işi geçti ve üç Linux paketini yayımladı (`.deb`, `.rpm`, AppImage) |
| **AniList canlı OAuth** | Geçerli `client_id`/`client_secret` yok; akış birim testleriyle doğrulandı, gerçek sunucuya karşı değil |
| **Gerçek sitelerin yüklenmesi** | Uygulama GUI'si başsız ortamda açılmıyor; WebView'da `openani.me`'nin gerçekten render olduğu görülmedi |
| **Site logoları** | OpenAnime ve Animecix karoları yerel dosya kullanır (`public/logos/`). İkisi de sitelerin kendi işaretleri; uydurma oynat simgesi kaldırıldı. Fotoğraf ekleme arayüzü yazıldı |

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

PiP tarafında script üç şey daha yapar (hepsi PR #19'un CI koşusunda gerçek proje
üzerinde çalıştı ve aarch64 debug APK üretti): `AnimeHubPlugin.kt`'yi kopyalar,
`AnimeHubPipController.kt`'yi şablondan üretir (şablondaki
`__ANIMEHUB_PIP_CONTROLLER_JS__` yer tutucusu, Rust'ın da enjekte ettiği
`src-tauri/src/web/pip_controller.js` ile doldurulur) ve üretilen
`MainActivity.kt`'ye `// >>> AnimeHub PiP lifecycle ... >>>` işaretleri
arasına PiP yaşam döngüsü bloğunu yazar. Blok her koşuda yenilenir, bu yüzden
`onDestroy` gibi üyeler iki kez oluşmaz. `MainActivity` `TauriActivity`'den
türemiyorsa (üretilen proje şekli değişmişse) script açıkça durur.
`./scripts/build.sh android` ve `release-android.yml` bunu kendisi çağırır.

`release-android.yml` içindeki iki gerçek hata da düzeltildi: keystore
dosyası `gen/android` oluşmadan yazılıyordu, ve `if:` içinde `secrets`
kullanılıyordu (Actions bunu reddeder).

Kotlin, Tauri 2 `JSObject.getInteger` / `getBoolean` / `put` imzalarına
göre yazıldı (kaynakla karşılaştırıldı) ve v0.2.0 itibarıyla **APK adımı
yeşil**: kırılan nokta, masaüstü Tauri API'lerinin (`add_child`,
`set_bounds`, `set_fullscreen`, `Webview::close`) Android hede fonculuğunda
tip olarak olmamasıydı; `open_site_window`, `close_site_window` ve
`install_main_window_handlers` artık `#[cfg(desktop)]` / `#[cfg(mobile)]`
varyantlarına sahip. Ayrıca `android_bridge.rs` plugin crate'in tipiyle aynı
isimde yerel struct tutuyordu (E0308) — `r.value` doğrudan okunuyor.

### B. Ayarlanmış profille Linux paketi

7 GB+ bellekli bir makinede:

```bash
npm run tauri build -- --bundles appimage deb rpm
```

`lto = true` + `codegen-units = 1` ile `cargo build --release --locked`
ubuntu-24.04 runner'da geçti (OOM yok). `.deb` / AppImage / rpm hâlâ üretilmedi.

### C. GitHub Actions'ı bir kez çalıştırmak

`ci.yml` PR'lerde çalışıyor: frontend testleri üç platformda, rustfmt,
clippy/test ve LTO linki, Android compile (aarch64) geçiyor.
`release-android.yml` ve `release-desktop.yml` v0.1.0'dan bu yana etiket
itmeliyeyle çalışıyor; v0.2.0 ile keystore akışı
(`secrets.ANDROID_KEYSTORE_BASE64` → geçici dosya → `shred -u`) da yeşil.

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
npm test                                # frontend (dosya listesi; `node --test tests/` yerelde Node 22.22.3'te çalışmıyor)
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
