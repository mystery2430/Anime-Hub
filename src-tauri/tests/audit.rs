//! Uçtan uca denetim testleri.
//!
//! Bu dosya üç şeyi kilitler:
//! 1. **Varsayılan siteler gerçekten çalışır** — kaydın ilk hali, politika
//!    doğrulaması ve başlatıcı görünüm modeli aynı adresler üzerinde uyuşur.
//! 2. **URL politikası saldırı vektörlerini reddeder** — her biri elle
//!    doğrulanmış gerçek çıktılara dayanır.
//! 3. **Gezinti guard'ı da özel adresleri keser** — bu, denetim sırasında
//!    bulunup düzeltilen açığın regresyon testidir.

use animehub_lib::sites::registry::{Category, Registry};
use animehub_lib::sites::url_policy::{
    is_private_host, sanitize_cookie_component, validate_site_url, Rejection,
};
use animehub_lib::web::dns::{answers_are_private, DnsClass};
use animehub_lib::web::session::{
    decide_navigation, decide_navigation_dns, embed_json, profile_dir_name, NavDecision,
    DNS_REBIND_BLOCK,
};
use url::Url;

fn blocklist() -> animehub_lib::sites::blocklist::Blocklist {
    animehub_lib::sites::blocklist::Blocklist::default()
}

// ------------------------------------------------------------------ 1. defaults

/// Kaydın ilk hali, kullanıcının dokunmadığı durumda tam olarak bu iki siteyi
/// içermeli. Bu test değişirse kullanıcı görünen davranış değişmiş demektir.
#[test]
fn default_sites_are_the_two_documented_ones() {
    let registry = Registry::new_default();
    let visible = registry.visible();

    assert_eq!(visible.len(), 2, "varsayılan site sayısı değişti");
    assert_eq!(visible[0].id, "builtin-openanime");
    assert_eq!(visible[0].name, "OpenAnime");
    assert_eq!(visible[0].url, "https://openani.me/");
    assert_eq!(visible[1].id, "builtin-animecix");
    assert_eq!(visible[1].name, "AnimeCix");
    assert_eq!(visible[1].url, "https://animecix.com/");

    for site in &visible {
        assert_eq!(site.category, Category::Watching);
        assert!(site.builtin, "{} yerleşik işaretli olmalı", site.id);
        assert!(!site.hidden, "{} varsayılan olarak görünür olmalı", site.id);
    }
}

/// Varsayılan siteler kendi güvenlik politikamızdan geçmek zorunda. Geçemezse
/// uygulama açılışta kendi engelini tetikler.
#[test]
fn default_sites_pass_their_own_url_policy() {
    for site in Registry::new_default().visible() {
        let safe = validate_site_url(&site.url)
            .unwrap_or_else(|e| panic!("{} politikayı geçemedi: {}", site.url, e.reason()));
        assert_eq!(
            safe.as_str(),
            site.url,
            "normalizasyon adresi değiştirmemeli"
        );
        assert!(
            !is_private_host(safe.host()),
            "{} özel adres sayıldı",
            site.url
        );
    }
}

/// Kayıttaki adres ile WebView'a giden adres aynı olmalı: normalizasyon bir
/// siteyi başka bir hosta kaydırmamalı.
#[test]
fn default_sites_navigate_to_their_own_host() {
    let bl = blocklist();
    for site in Registry::new_default().visible() {
        let url = Url::parse(&site.url).expect("varsayılan adres çözümlenmeli");
        assert_eq!(
            decide_navigation(&url, url.host_str().unwrap(), &bl),
            NavDecision::Allow,
            "{} kendi guard'ımız tarafından engellendi",
            site.url
        );
    }
}

// ------------------------------------------------------------------ 2. url policy

/// Elle çalıştırılıp çıktısı doğrulanmış vektörler. Liste genişletilirken
/// gerçek davranışı görmek için `cargo test -- --nocapture` kullanın.
#[test]
fn url_policy_rejects_known_attack_vectors() {
    let denied: &[(&str, Rejection)] = &[
        ("http://openani.me/", Rejection::InsecureScheme),
        ("javascript:alert(1)", Rejection::InsecureScheme),
        ("data:text/html,<script>", Rejection::InsecureScheme),
        ("file:///etc/passwd", Rejection::InsecureScheme),
        ("blob:https://a/b", Rejection::InsecureScheme),
        ("ftp://openani.me/", Rejection::NotHttpLike),
        ("openani.me", Rejection::Unparsable),
        ("", Rejection::Unparsable),
        // Yerel/ağ içi hedefler: yönlendiriciler ve bulut metadata uçları.
        ("https://127.0.0.1/", Rejection::LocalOrPrivateHost),
        ("https://2130706433/", Rejection::LocalOrPrivateHost), // ondalık 127.0.0.1
        ("https://0x7f.1/", Rejection::LocalOrPrivateHost),     // onaltılık 127.0.0.1
        ("https://localhost/", Rejection::LocalOrPrivateHost),
        ("https://[::1]/", Rejection::LocalOrPrivateHost),
        ("https://[::ffff:127.0.0.1]/", Rejection::LocalOrPrivateHost),
        ("https://192.168.1.1/", Rejection::LocalOrPrivateHost),
        ("https://10.0.0.1/", Rejection::LocalOrPrivateHost),
        ("https://172.16.0.1/", Rejection::LocalOrPrivateHost),
        ("https://169.254.169.254/", Rejection::LocalOrPrivateHost),
        ("https://0.0.0.0/", Rejection::LocalOrPrivateHost),
        ("https://foo.local/", Rejection::LocalOrPrivateHost),
        ("https://evil.internal/", Rejection::LocalOrPrivateHost),
        ("https://tauri.localhost/", Rejection::LocalOrPrivateHost),
        // Kullanıcı adı/parola: kimlik bilgisi sızıntısı ve host karmaşası.
        ("https://openani.me@evil.com/", Rejection::CredentialsInUrl),
        ("https://user:pass@openani.me/", Rejection::CredentialsInUrl),
    ];

    for (raw, expected) in denied {
        match validate_site_url(raw) {
            Err(got) => assert_eq!(got, *expected, "{raw} için yanlış ret nedeni"),
            Ok(u) => panic!("{raw} kabul edildi -> {}", u.as_str()),
        }
    }
}

/// Kabul edilmesi gerekenler: bunlar reddedilirse gerçek siteler açılmaz.
#[test]
fn url_policy_accepts_legitimate_urls() {
    let allowed = &[
        ("https://openani.me/", "openani.me"),
        ("https://openani.me", "openani.me"), // kök yol eklenir
        ("https://openani.me:8080/", "openani.me"),
        ("https://openani.me/watch#bolum-3", "openani.me"), // fragment düşer
        ("https://www.animecix.com/x", "www.animecix.com"),
        ("  https://openani.me/  ", "openani.me"), // boşluk kırpılır
    ];

    for (raw, host) in allowed {
        let safe =
            validate_site_url(raw).unwrap_or_else(|e| panic!("{raw} reddedildi: {}", e.reason()));
        assert_eq!(safe.host(), *host, "{raw} yanlış hosta çözüldü");
        assert!(safe.as_str().starts_with("https://"), "{raw} https değil");
    }
}

/// `#` sonrası her şey düşürülür; `@` ile host karıştırılamaz.
#[test]
fn fragment_and_backslash_cannot_swap_the_host() {
    let safe = validate_site_url("https://openani.me#@evil.com").expect("kabul edilmeli");
    assert_eq!(safe.host(), "openani.me");
    assert_eq!(safe.as_str(), "https://openani.me/");

    // `\@` url crate'i tarafından yola çevrilir: host yine kullanıcının
    // yazdığı host olur, yani "görünenden farklı yere gitme" olmaz.
    let safe = validate_site_url(r"https://evil.com\@openani.me/").expect("kabul edilmeli");
    assert_eq!(safe.host(), "evil.com");
}

// ------------------------------------------------------------------ 3. guard

/// **Regresyon:** denetimde bulunan açık. `decide_navigation` yalnızca şemaya
/// bakıyordu; bir site WebView'ı `https://192.168.1.1/` adresine
/// yönlendirebiliyordu. Artık kesilmeli.
#[test]
fn navigation_guard_blocks_private_hosts() {
    let bl = blocklist();
    for raw in [
        "https://192.168.1.1/",
        "https://127.0.0.1/admin",
        "https://169.254.169.254/latest/meta-data/",
        "https://10.0.0.1/",
        "https://localhost/",
        "https://foo.local/",
        "https://[::1]/",
    ] {
        let url = Url::parse(raw).expect("test adresi çözümlenmeli");
        assert!(
            matches!(
                decide_navigation(&url, "openani.me", &bl),
                NavDecision::Block(_)
            ),
            "{raw} gezinti guard'ından geçti"
        );
    }
}

/// **Regresyon:** DNS rebinding. Metin politikası `rebind.example` adresini
/// herkese açık sayar; çözümleme özel bir adres döndürürse guard kesmeli.
/// Çözümleyici cevap veremezse (zaman aşımı) metin politikası geçerli kalır —
/// DNS kesintisi bütün siteleri kapatmamalı.
#[test]
fn navigation_guard_blocks_dns_rebinding() {
    let bl = blocklist();
    let url = Url::parse("https://rebind.example/latest/meta-data/").unwrap();
    assert_eq!(
        decide_navigation_dns(&url, "openani.me", &bl, DnsClass::Answered { private: true }),
        NavDecision::Block(DNS_REBIND_BLOCK)
    );
    assert_eq!(
        decide_navigation(&url, "openani.me", &bl),
        NavDecision::Allow,
        "string policy alone must still allow a public-looking name"
    );
    assert!(answers_are_private(&[
        "1.1.1.1".parse().unwrap(),
        "169.254.169.254".parse().unwrap(),
    ]));
    assert!(!answers_are_private(&["1.1.1.1".parse().unwrap()]));
}

/// Guard'ın normal gezintiyi kesmemesi gerekir; yoksa siteler bozulur.
#[test]
fn navigation_guard_allows_public_https() {
    let bl = blocklist();
    for raw in [
        "https://openani.me/",
        "https://cdn.openani.me/a.js",
        "https://example.com:8443/x",
    ] {
        let url = Url::parse(raw).expect("test adresi çözümlenmeli");
        assert_eq!(
            decide_navigation(&url, "openani.me", &bl),
            NavDecision::Allow,
            "{raw} yanlışlıkla engellendi"
        );
    }
}

// ------------------------------------------------------------------ 4. injection

/// Enjekte edilen script'in tek veri kanalı. `</script>` erken kapatmamalı.
#[test]
fn embed_json_cannot_break_out_of_the_script_block() {
    #[derive(serde::Serialize)]
    struct P {
        a: String,
    }

    for payload in [
        "</script><script>alert(1)</script>",
        "</SCRIPT >",
        "a\u{2028}b",
        "a\u{2029}b",
        "\"\\'`",
        "\\u003c/script\\u003e",
    ] {
        let out = embed_json(&P { a: payload.into() });
        assert!(
            !out.contains("</script"),
            "script bloğu erken kapanıyor: {out}"
        );
        assert!(!out.contains('\u{2028}'), "ham U+2028 sızdı: {out}");
        assert!(!out.contains('\u{2029}'), "ham U+2029 sızdı: {out}");
        // Geçerli JSON olarak kalmalı, aksi halde enjekte edilen script patlar.
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("embed_json geçerli JSON üretmeli");
        assert_eq!(parsed["a"].as_str(), Some(payload));
    }
}

/// Profil dizini adı dosya yoluna yazılıyor: `..` ile dışarı çıkılamamalı.
#[test]
fn profile_dir_name_cannot_escape_its_parent() {
    for input in [
        "builtin-openanime",
        "../../etc/passwd",
        "..",
        "/",
        "a/b",
        "a\x00b",
        "",
        "   ",
    ] {
        let dir = profile_dir_name(input);
        assert!(
            !dir.contains(".."),
            "{input:?} -> {dir:?} üst dizine çıkıyor"
        );
        assert!(
            !dir.contains('/'),
            "{input:?} -> {dir:?} yol ayracı içeriyor"
        );
        assert!(!dir.contains('\0'), "{input:?} -> {dir:?} NUL içeriyor");
        assert!(!dir.is_empty(), "{input:?} boş dizin adı üretti");
    }
    // Aynı girdi aynı adı vermeli, yoksa oturum verisi kaybolur.
    assert_eq!(
        profile_dir_name("builtin-openanime"),
        profile_dir_name("builtin-openanime")
    );
    assert_ne!(profile_dir_name("a"), profile_dir_name("b"));
}

/// Çerez bileşenleri başlığa yazılıyor: CRLF enjeksiyonu kesilmeli.
#[test]
fn cookie_components_cannot_inject_headers() {
    for input in [
        "a\r\nSet-Cookie: evil=1",
        "a\nb",
        "a;b=c",
        "a,b",
        "a b",
        "a\"b",
        "a\\b",
    ] {
        let out = sanitize_cookie_component(input);
        assert!(
            !out.contains('\r') && !out.contains('\n'),
            "{input:?} -> {out:?} CRLF tuttu"
        );
        assert!(
            !out.contains(';'),
            "{input:?} -> {out:?} noktalı virgül tuttu"
        );
        assert!(!out.contains(' '), "{input:?} -> {out:?} boşluk tuttu");
    }
}
