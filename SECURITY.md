# Güvenlik Politikası

AnimeHub bir WebView kabuğudur: siteleri izole oturumlarda açar, çerezleri ve
sayfa depolarını yerel olarak şifreler, hiçbir içeriği barındırmaz veya
indirmez. Güvenlik bildirimleri bu yüzden önceliklidir.

## Desteklenen sürümler

| Sürüm | Güvenlik düzeltmesi |
|---|---|
| `0.3.x` (en son yayın) | ✅ |
| Daha eski sürümler | ❌ — lütfen önce en son sürüme güncelleyin |

## Açık bildirimi

Lütfen **herkese açık issue açmayın**. Özel bildirim için:

1. GitHub üzerinde **Security → Report a vulnerability** (özel danışma) —
   <https://github.com/mystery2430/Anime-Hub/security/advisories/new>
2. Mümkünse ekleyin: sürüm numarası (`Ayarlar → Hakkında`), işletim sistemi,
   yeniden üretme adımları, beklenen ve gözlenen davranış.

**Gizli bilgi paylaşmayın:** erişim belirteci, çerez, anahtar, keystore parolası
veya kişisel veri içeren ekran görüntüsü/log göndermeyin. Gerekirse değerleri
`***` ile maskeleyin.

## Kapsam

Özellikle ilgilendiğimiz konular:

- WebView izolasyonunun aşılması (çerez/depo sızıntısı, origin karışması),
- şifreli depo veya anahtar yönetimi (DPAPI, Android Keystore, Secret Service),
- gezinme politikası ve engelleme listesi atlatmaları (ör. `javascript:`/`data:`
  şemaları, kimlik bilgisi gömülü adresler, yerel/ağ içi adresler),
- derleme/yayın zinciri (imzalama, CI sırları, bağımlılık kaynağı),
- Deep-link ve dış bağlantı işleme.

Kapsam dışı: sitedeki içerik ve site hesapları (üçüncü taraf hizmetler),
kullanıcının kendi cihazındaki kötü amaçlı yazılım, sosyal mühendislik.

## Yanıt süreci

- İlk yanıt hedefi: **72 saat** içinde.
- Doğrulanan bildirimler için düzeltme, en kısa sürede yeni bir yama sürümünde
  yayınlanır; bildirim sahibi dilerse sürüm notlarında anılır.
- Bildirimi çözümlenmeden önce kamuya açıklamamanızı rica ederiz (koordineli
  açıklama).
