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
            commands::get_site_cookies,
            commands::set_site_cookies,
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
            if url.scheme() != OAUTH_SCHEME || url.host_str() != Some("anilist") {
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
    use crate::android_bridge::SiteProfileAction;
    use crate::web::windows as w;
    let pending = w::read_pending_profile_deletes(state)?;
    if pending.is_empty() {
        return Ok(true);
    }
    log::info!("ertelenen profil silmeleri deneniyor: {}", pending.len());
    for name in &pending {
        match crate::android_bridge::site_profile_delete(name) {
            Ok(SiteProfileAction::Deleted) | Ok(SiteProfileAction::Absent) => {
                w::forget_pending_profile_delete(state, name)?;
                log::info!("ertelenen profil verisi silindi");
            }
            Ok(SiteProfileAction::Pending) => {
                // Still in use or still unsupported: keep it queued for a
                // later start. Never reported to the user as deleted.
                log::warn!("ertelenen profil silme bir kez daha ertelendi");
            }
            Err(e) => {
                log::warn!("ertelenen profil silme denenemedi (köprü hazır değil): {e}");
                return Ok(false);
            }
        }
    }
    Ok(true)
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
    format!("{OAUTH_SCHEME}://{OAUTH_CALLBACK_PATH}?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_url_uses_the_registered_scheme() {
        let u = mobile_callback_url("code=abc&state=s1");
        assert_eq!(u, "animehub://anilist/callback?code=abc&state=s1");
        // Must be parseable by the same parser the command uses.
        let (code, state) = crate::anilist::parse_callback(&u).unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "s1");
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
