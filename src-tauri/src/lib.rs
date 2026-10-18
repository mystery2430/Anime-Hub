//! AnimeHub — a launcher that opens anime tracking and streaming sites in
//! isolated, encrypted WebView sessions.
//!
//! Module map:
//! * [`anilist`] — clean-room AniList OAuth + GraphQL client
//! * [`android_bridge`] — Android Keystore and Picture-in-Picture
//! * [`commands`] — the `invoke` surface
//! * [`secure`] — AES-GCM envelopes, platform key sources, document store
//! * [`sites`] — URL policy, site registry, blocklist
//! * [`web`] — WebView session isolation and window policy

pub mod android_bridge;
pub mod anilist;
pub mod commands;
pub mod error;
pub mod secure;
pub mod sites;
pub mod web;

use commands::AppState;
use error::AppResult;
use secure::master_key::load_master_key;
use secure::store::SecureStore;
use secure::SecretProvider;
use sites::blocklist::Blocklist;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex;
use tauri::{Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;

/// Deep-link scheme registered for the AniList OAuth redirect on mobile.
pub const OAUTH_SCHEME: &str = "animehub";
/// Host+path of that redirect.
pub const OAUTH_CALLBACK_PATH: &str = "anilist/callback";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        // The Keystore + PiP bridge only exists on Android; on every other
        // target it would register a plugin whose commands can never work.
        .plugin(animehub_android::init())
        .setup(setup)
        .invoke_handler(tauri::generate_handler![
            commands::list_sites,
            commands::add_site,
            commands::update_site,
            commands::remove_site,
            commands::set_site_hidden,
            commands::open_site,
            commands::close_site,
            commands::clear_site_data,
            commands::get_settings,
            commands::update_settings,
            commands::reset_blocklist,
            commands::take_startup_warning,
            commands::anilist_login_start,
            commands::anilist_login_callback,
            commands::anilist_status,
            commands::anilist_hero_stats,
            commands::anilist_logout,
            commands::anilist_library,
            commands::anilist_search,
            commands::anilist_update,
            commands::anilist_delete,
            commands::enter_pip,
            commands::set_pip_auto_enter,
            commands::debug_log,
            commands::set_window_theme,
            commands::app_info,
        ])
        .run(tauri::generate_context!())
        .expect("AnimeHub çalıştırılamadı");
}

/// Build the app state: load the platform key, open the encrypted store, and
/// read the registry and settings out of it.
fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&config_dir)?;

    let provider: Box<dyn SecretProvider> = build_provider(&config_dir)?;

    let (registry, warning) = provider.load_registry();
    // `load_doc` is a generic provided method, so it is not callable through
    // `dyn`; decode the bytes here instead.
    let settings: commands::Settings = provider
        .load_doc_bytes("settings.bin")
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();

    let blocklist = Blocklist::from_rules(&settings.blocklist.rules, settings.blocklist.enabled);
    // Captured before `settings` moves into the managed state; applied to
    // the native window below.
    let theme_pref = settings.theme;

    // Android: the controlled PiP paths read this before they may enter PiP,
    // and the native controller starts out "off" in a fresh process. The
    // setting otherwise only reaches it when the user toggles the switch, so a
    // restart would silently disable auto-PiP.
    //
    // Throwaway thread, not the async runtime: the call blocks on the Kotlin
    // side and the plugin bridge may not answer in the first moments of the
    // process, so this retries a few times instead of blocking startup. A
    // failure here is only a warning — the toggle keeps working.
    //
    // The startup value and the user toggle share `pip_lock`+`pip_generation`
    // in `AppState`. The retry loop holds `pip_lock` across the live check
    // and the native call, so a toggle that wins the lock first cannot be
    // overwritten by a stale startup value later. Generation makes even a
    // value-preserving toggle (false→true→false) visible as "changed".
    #[cfg(target_os = "android")]
    {
        let pip_auto_enter = settings.pip_auto_enter;
        // Generation at startup is 0 – the store has just been loaded.
        let pip_generation = 0u64;
        let handle = app.handle().clone();
        std::thread::spawn(move || {
            for attempt in 0..4 {
                let state = handle.try_state::<AppState>();
                if let Some(s) = state {
                    // Try to hold `pip_lock` across the live check and the native call.
                    if let Ok(_guard) = s.pip_lock.try_lock() {
                        let live_gen = s.pip_generation.load(std::sync::atomic::Ordering::SeqCst);
                        let live_val = s
                            .settings
                            .lock()
                            .map(|g| g.pip_auto_enter)
                            .unwrap_or(pip_auto_enter);
                        if !crate::commands::pip_startup_should_apply(
                            pip_generation,
                            live_gen,
                            pip_auto_enter,
                            live_val,
                        ) {
                            log::debug!(
                                "PiP startup skipped (live {live_val} gen {live_gen} != startup {pip_auto_enter} gen {pip_generation})"
                            );
                            return;
                        }
                        // `_guard` stays alive while we call the native side.
                        match crate::android_bridge::set_pip_auto_enter(pip_auto_enter) {
                            Ok(applied) => {
                                log::debug!("PiP auto-enter ayarı uygulandı: {applied}");
                                return;
                            }
                            Err(e) if attempt == 3 => {
                                log::warn!("PiP auto-enter ayarı uygulanamadı: {e}");
                                return;
                            }
                            Err(_) => {
                                // Drop guard before sleeping and retry.
                            }
                        }
                    } else {
                        // Another PiP apply holds the lock – a toggle is in flight.
                        // Stale startup must not overwrite it; skip.
                        log::debug!("PiP startup skipped (pip_lock busy)");
                        return;
                    }
                } else {
                    // State not yet available – fall back to direct native call.
                    match crate::android_bridge::set_pip_auto_enter(pip_auto_enter) {
                        Ok(applied) => {
                            log::debug!("PiP auto-enter ayarı uygulandı: {applied}");
                            return;
                        }
                        Err(e) if attempt == 3 => {
                            log::warn!("PiP auto-enter ayarı uygulanamadı: {e}");
                            return;
                        }
                        Err(_) => {}
                    }
                }
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
        });
    }

    if let Some(w) = &warning {
        log::error!("kayıtlı site listesi okunamadı, varsayılanlara dönüldü: {w}");
    }
    // The raw error stays in the log. The notice is a fixed sentence so a
    // parse fragment never reaches the launcher.
    let startup_warning = warning
        .as_ref()
        .map(|_| "Kayıtlı site listesi okunamadı, varsayılanlara dönüldü.".to_string());
    if !provider.backend().is_os_backed() {
        log::warn!(
            "işletim sistemi anahtar deposu bulunamadı; anahtar {} olarak saklanıyor",
            provider.backend().label()
        );
    }

    // Remember where the launcher lives so mobile can navigate back to it.
    let launcher_url = app.get_webview_window("main").and_then(|w| w.url().ok());

    app.manage(AppState {
        provider,
        registry: Mutex::new(registry),
        settings: Mutex::new(settings),
        blocklist: Mutex::new(blocklist),
        oauth_state: Mutex::new(None),
        startup_warning: Mutex::new(startup_warning),
        launcher_url: Mutex::new(launcher_url),
        current_site: Mutex::new(None),
        pip_lock: Mutex::new(()),
        pip_generation: AtomicU64::new(0),
    });

    // Android: finish deferred profile deletions before the frontend can
    // open anything (see `drain_pending_profile_deletes`). A throwaway
    // thread, like the PiP startup apply: the Kotlin bridge may not answer
    // in the first moments of the process, so failures are retried a few
    // times and then left for the next start — the queue never loses an
    // entry because of a slow boot.
    #[cfg(target_os = "android")]
    {
        let handle = app.handle().clone();
        std::thread::spawn(move || {
            for attempt in 0..8 {
                let stop = match handle.try_state::<AppState>() {
                    Some(state) => match crate::web::windows::lock_android_session() {
                        Ok(_guard) => match drain_pending_profile_deletes(&state) {
                            Ok(done) => done,
                            Err(e) => {
                                // Storage failure: retrying now would fail the
                                // same way; the queue stays for the next start.
                                log::error!("ertelenen profil silmeleri okunamadı: {e}");
                                true
                            }
                        },
                        Err(e) => {
                            log::error!("ertelenen profil silmeleri kilitlenemedi: {e}");
                            false
                        }
                    },
                    None => false,
                };
                if stop {
                    return;
                }
                if attempt < 7 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
            log::warn!(
                "ertelenen profil silmeleri bu açılışta işlenemedi; sonraki açılışta yeniden denenecek"
            );
        });
    }

    // Route the OAuth redirect into the login command on every platform.
    //
    // On desktop the scheme is not registered by the OS installer, so do it
    // at startup from the `plugins.deep-link` config; on Android the
    // intent-filter in AndroidManifest.xml handles it instead.
    #[cfg(desktop)]
    if let Err(e) = app.deep_link().register_all() {
        log::warn!("deep-link şeması kaydedilemedi: {e}");
    }

    // Desktop hosts site webviews inside the launcher window: keep them
    // glued to the window size from the first resize on.
    crate::web::windows::install_main_window_handlers(&app.handle().clone());

    // Apply the saved theme override to the native window. `System` maps to
    // `None`: the OS decides, and both the window chrome and the WebViews'
    // `prefers-color-scheme` follow it.
    if let Some(w) = app.get_webview_window("main") {
        let target = match theme_pref {
            commands::ThemePref::System => None,
            commands::ThemePref::Dark => Some(tauri::Theme::Dark),
            commands::ThemePref::Light => Some(tauri::Theme::Light),
        };
        if let Err(e) = w.set_theme(target) {
            log::warn!("kayıtlı tema uygulanamadı: {e}");
        }
    }

    let handle = app.handle().clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            // Exact registered redirect only; see `is_registered_mobile_callback`.
            if !is_registered_mobile_callback(url.as_ref()) {
                log::warn!(
                    "kayıtlı olmayan deep-link yok sayıldı (şema: {})",
                    url.scheme()
                );
                continue;
            }
            let handle = handle.clone();
            let callback = url.to_string();
            // The command is async; spawn it rather than blocking the event.
            tauri::async_runtime::spawn(async move {
                let handle2 = handle.clone();
                match crate::commands::anilist_login_callback(handle, callback).await {
                    Ok(user) => {
                        let _ = handle2.emit("animehub://anilist-login", user.name);
                    }
                    Err(e) => {
                        let _ = handle2.emit("animehub://error", e.to_string());
                    }
                }
            });
        }
    });

    Ok(())
}

/// Drain the deferred Android profile-deletion queue for one round.
///
/// Returns `Ok(true)` when the queue needs no further attempts this process
/// (emptied, or the remainder only awaits a later start), `Ok(false)` when
/// the plugin bridge was not ready yet and the caller should retry.
///
/// Chromium refuses to delete a profile that is loaded in the current
/// process (`AwBrowserContextStore::Delete` returns in-use), and a provider
/// without `MULTI_PROFILE` cannot delete at all — both outcomes were queued
/// by `remove_site` instead of being reported as a success. Draining runs at
/// startup *before* any site can open, so a queued name can never be
/// re-created while its on-disk deletion is still pending. A name that still
/// refuses to delete stays queued; the "clear all" orphan sweep in
/// `clear_site_data` is the second safety net.
#[cfg(target_os = "android")]
fn drain_pending_profile_deletes(state: &crate::commands::AppState) -> AppResult<bool> {
    use crate::error::AppError;
    use crate::web::windows as w;

    let entries = w::read_pending_profile_delete_entries(state)?;
    if entries.is_empty() {
        return Ok(true);
    }
    log::info!("ertelenen profil silmeleri deneniyor: {}", entries.len());

    // Registered site ids, collected once and WITHOUT holding the lock across
    // a provider call. A tombstone an older build wrote as a bare name has no
    // recorded owner, and a profile name is a one-way digest of the site id —
    // so re-matching against the registry is the only way to find out whose
    // generation has to be secured before the tombstone may be lifted.
    let site_ids: Vec<String> = {
        let reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        reg.sites.iter().map(|site| site.id.clone()).collect()
    };

    // The order (delete → prove the name is underivable → lift → retire) lives
    // in the host-tested `run_pending_delete_drain`, not here: `cargo test`
    // proves it on every OS instead of leaving it to the aarch64 compile.
    let report = w::run_pending_delete_drain(
        state,
        &entries,
        crate::android_bridge::site_profile_delete,
        crate::android_bridge::site_profile_retire,
        |name| w::match_queued_name_to_site(name, &site_ids),
    )?;

    for name in &report.kept_unmatched {
        log::warn!(
            "ertelenen profil silmesi kuyrukta korundu (sahibi belirsiz veya nesil güvenceye alınamadı): {name}"
        );
    }
    if report.lifted > 0 || report.deferred > 0 || report.unsupported > 0 {
        log::info!(
            "ertelenen profil silmeleri: {} kaldırıldı, {} ertelendi, {} desteklenmiyor, {} korundu",
            report.lifted,
            report.deferred,
            report.unsupported,
            report.kept_unmatched.len()
        );
    }
    Ok(!report.bridge_not_ready)
}

/// Choose the secret backend for this platform.
fn build_provider(config_dir: &std::path::Path) -> AppResult<Box<dyn SecretProvider>> {
    #[cfg(target_os = "android")]
    {
        return Ok(Box::new(secure::AndroidStore::new(config_dir)?));
    }

    #[cfg(not(target_os = "android"))]
    {
        let key = load_master_key(config_dir)?;
        Ok(Box::new(SecureStore::new(&key, config_dir)?))
    }
}

/// Full callback URL for the mobile OAuth redirect.
pub fn mobile_callback_url(query: &str) -> String {
    format!("{}?{query}", mobile_redirect_uri())
}

/// The exact redirect URI the mobile deep link is registered for.
///
/// Built from the same constants that build the callback URL, so the value the
/// OAuth parser compares against and the value the OS intent filter matches
/// cannot drift apart. It must stay equal to `plugins.deep-link.mobile` in
/// `tauri.conf.json` (`scheme: animehub`, `host: anilist`,
/// `pathPrefix: /callback`) — `tests/security.test.js` pins that.
pub fn mobile_redirect_uri() -> String {
    format!("{OAUTH_SCHEME}://{OAUTH_CALLBACK_PATH}")
}

/// True when `raw` is the registered mobile redirect URI (optionally with a
/// query), and nothing else.
///
/// WHY A SEPARATE FUNCTION: the deep-link closure can only run on a device, so
/// its decision is extracted here where host tests can exercise it. The old
/// filter compared scheme and host only, which let `animehub://anilist/other`
/// or `animehub://anilist/callback/extra` reach the login command; the command
/// now rejects those too (`anilist::parse_callback`), but an intent that is not
/// ours should not start a token exchange at all.
pub fn is_registered_mobile_callback(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let Ok(expected) = url::Url::parse(&mobile_redirect_uri()) else {
        return false;
    };
    url.scheme() == expected.scheme()
        && url.host_str() == expected.host_str()
        && url.path() == expected.path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_url_uses_the_registered_scheme() {
        let u = mobile_callback_url("code=abc&state=s1");
        assert_eq!(u, "animehub://anilist/callback?code=abc&state=s1");
        // Must be parseable by the same parser the command uses, against the
        // same redirect URI the command will pass in.
        let (code, state) = crate::anilist::parse_callback(&u, &mobile_redirect_uri()).unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "s1");
    }

    #[test]
    fn mobile_intent_filter_accepts_only_the_registered_redirect() {
        assert!(is_registered_mobile_callback(&mobile_redirect_uri()));
        assert!(is_registered_mobile_callback(&mobile_callback_url(
            "code=a&state=b"
        )));
        assert!(is_registered_mobile_callback(
            "ANIMEHUB://anilist/callback?code=a&state=b"
        ));

        // A hijacked or merely similar intent must not reach the login command.
        for raw in [
            "animehub://evil/callback?code=a&state=b",
            "animehub://anilist/other?code=a&state=b",
            "animehub://anilist/callback/extra?code=a&state=b",
            "animehub://anilist?code=a&state=b",
            "animehub:/anilist/callback?code=a&state=b",
            "https://anilist/callback?code=a&state=b",
            "http://127.0.0.1:17395/callback?code=a&state=b",
            "",
            "not a url",
            "//anilist/callback?code=a&state=b",
        ] {
            assert!(!is_registered_mobile_callback(raw), "{raw} kabul edildi");
        }
    }

    #[test]
    fn registered_mobile_redirect_matches_the_deep_link_config() {
        // The runtime intent filter and `tauri.conf.json` must describe the same
        // redirect, or the OS hands the callback to nobody.
        let conf = include_str!("../tauri.conf.json");
        let conf: serde_json::Value = serde_json::from_str(conf).expect("tauri.conf.json");
        let mobile = &conf["plugins"]["deep-link"]["mobile"];
        let entries = mobile.as_array().expect("mobile deep-link listesi");
        // In tauri.conf.json both `scheme` and `pathPrefix` are LISTS
        // (`"scheme": ["animehub"]`, `"pathPrefix": ["/callback"]`), which is
        // what the generated Android intent filter is built from. Reading them
        // as strings silently matched nothing.
        let entry = entries
            .iter()
            .find(|e| {
                e["scheme"].as_array().is_some_and(|list| {
                    list.iter()
                        .any(|scheme| scheme.as_str() == Some(OAUTH_SCHEME))
                })
            })
            .expect("animehub şeması kayıtlı olmalı");
        let uri = mobile_redirect_uri();
        assert!(uri.starts_with(&format!("{OAUTH_SCHEME}://")), "{uri}");
        let host = entry["host"]
            .as_str()
            .expect("deep-link host'u bir dize olmalı");
        assert!(uri.contains(host), "{uri} config host'u içermeli: {host}");
        let prefixes: Vec<&str> = entry["pathPrefix"]
            .as_array()
            .expect("pathPrefix bir liste olmalı")
            .iter()
            .filter_map(|prefix| prefix.as_str())
            .collect();
        assert!(!prefixes.is_empty(), "pathPrefix listesi boş");
        assert!(
            prefixes.iter().any(|prefix| uri.ends_with(prefix)),
            "{uri} config pathPrefix değerlerinden biriyle bitmeli: {prefixes:?}"
        );
        // ... and the OAuth parser agrees with the intent filter.
        let callback = mobile_callback_url("code=a&state=b");
        assert!(crate::anilist::parse_callback(&callback, &uri).is_ok());
        assert!(crate::anilist::parse_callback(&callback, "animehub://evil/callback").is_err());
    }

    #[test]
    fn provider_loads_from_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let provider = build_provider(dir.path()).expect("provider");
        let (reg, warning) = provider.load_registry();
        assert_eq!(reg.sites.len(), 2);
        assert!(warning.is_none());
        // Settings must default rather than fail on first run.
        let settings: Option<commands::Settings> = provider
            .load_doc_bytes("settings.bin")
            .unwrap()
            .map(|b| serde_json::from_slice(&b).unwrap());
        assert!(settings.is_none());
    }

    #[test]
    fn provider_persists_a_site_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        {
            let p = build_provider(dir.path()).unwrap();
            let mut reg = p.load_registry().0;
            reg.add(crate::sites::registry::SiteDraft {
                name: "Kalıcı".into(),
                url: "https://kalici.example/".into(),
                category: crate::sites::registry::Category::Watching,
                letter: None,
                color: None,
                image: None,
            })
            .unwrap();
            p.save_registry(&reg).unwrap();
        }
        let p2 = build_provider(dir.path()).unwrap();
        let (reg, _) = p2.load_registry();
        assert!(reg.sites.iter().any(|s| s.name == "Kalıcı"));
    }
}

// ---------------------------------------------------------------------------
// IPC surface
// ---------------------------------------------------------------------------

#[cfg(test)]
fn frontend_sources() -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("js") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../src")),
        &mut out,
    );
    out
}

#[cfg(test)]
mod ipc_surface_tests {
    /// Commands that must stay OFF the invoke surface.
    ///
    /// Both were reachable from any page in the launcher WebView and took the
    /// site id / whole jar from the caller, i.e. they let the frontend read or
    /// overwrite every site's encrypted cookie jar with no site ownership,
    /// profile-generation or quarantine check in the path. Nothing in `src/`,
    /// the mobile plugin or any other Rust path called them (the 0.3.4 jar
    /// import lives inside `web::windows`), so they are gone rather than
    /// gated.
    #[test]
    fn raw_cookie_jar_commands_are_gone_from_the_ipc_surface() {
        let handlers = include_str!("lib.rs");
        let commands = include_str!("commands.rs");
        let mut checked = 0usize;

        for prefix in ["get", "set"] {
            // Built at runtime on purpose: writing the literal name in this
            // test would make `contains` match the assertion itself.
            let name = format!("{prefix}_site_cookies");
            checked += 1;

            let registration = format!("commands::{name}");
            assert!(
                !handlers.contains(&registration),
                "{name} hâlâ invoke_handler'da kayıtlı"
            );
            let definition = format!("pub fn {name}");
            assert!(
                !commands.contains(&definition),
                "{name} hâlâ bir komut olarak tanımlı"
            );
            let frontend = crate::frontend_sources();
            assert!(!frontend.is_empty(), "frontend kaynakları bulunamadı");
            for path in frontend {
                let text = std::fs::read_to_string(&path).expect("kaynak okunabilmeli");
                assert!(
                    !text.contains(&name),
                    "{} {name} komutuna başvuruyor (çağrı, alias, ne olursa)",
                    path.display()
                );
            }
        }
        assert_eq!(checked, 2, "iki komut da denetlenmeli");
    }

    /// The jar itself is still used where it belongs: the 0.3.4 import and the
    /// mobile open/clear/delete protocols read and write it directly through
    /// the storage provider.
    #[test]
    fn cookie_jar_storage_keys_still_have_a_production_user() {
        let windows = include_str!("web/windows.rs");
        // Built at runtime so this assertion does not match its own text.
        let needle = format!("cookies-{}", "{");
        assert!(
            windows.contains(&needle),
            "web/windows.rs site çerez anahtarını üretmeli"
        );
        let jar = include_str!("web/session.rs");
        assert!(jar.contains("pub struct CookieJar"), "CookieJar durmalı");
    }
}
