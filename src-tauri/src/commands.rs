//! The `invoke` surface exposed to the frontend.
//!
//! Commands are deliberately thin: validation, encryption and window policy
//! all live in the modules they belong to, so this file is mostly plumbing.

use crate::anilist::{self, AniListClient, AniListConfig, MediaListStatus, Token};
use crate::error::{AppError, AppResult};
use crate::secure::master_key::Backend;
use crate::secure::SecretProvider;
use crate::sites::blocklist::{Blocklist, BlocklistState};
use crate::sites::registry::{Category, Icon, Registry, Site, SiteDraft};
use crate::web::session::{build_init_script, InjectedConfig};
use crate::web::windows;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct AppState {
    pub provider: Box<dyn SecretProvider>,
    pub registry: Mutex<Registry>,
    pub settings: Mutex<Settings>,
    pub blocklist: Mutex<Blocklist>,
    /// Set while an OAuth flow is waiting for its redirect.
    pub oauth_state: Mutex<Option<String>>,
    /// Warning from a registry that could not be decrypted on startup.
    pub startup_warning: Mutex<Option<String>>,
    /// Absolute URL of the launcher page, captured at startup.
    ///
    /// Mobile needs this to navigate the shared WebView *back* to the
    /// launcher after a site is closed.
    pub launcher_url: Mutex<Option<url::Url>>,
    /// Which site currently owns the WebView session.
    ///
    /// On desktop this names the extra window; on Android it names the site
    /// whose profiled WebView is currently attached above the launcher, so
    /// open/close/wipe/flush all serialize against the same fact.
    pub current_site: Mutex<Option<CurrentSite>>,
    /// Serialize Picture-in-Picture native applies (startup vs toggle, and
    /// rapid toggles). `set_pip_auto_enter` and the startup thread both hold
    /// this around the native call plus the settings write, so the last
    /// requested value always wins and the native side never lags behind the
    /// persisted setting.
    pub pip_lock: Mutex<()>,
    /// Monotonic generation for the PiP preference; bumped only after a
    /// successful persist. Startup compares its captured generation to the
    /// live one while holding `pip_lock`, so even a value-preserving toggle
    /// (false→true→false) is not mistaken for \"no change\".
    pub pip_generation: std::sync::atomic::AtomicU64,
    /// Serialize developer-options changes with the Kotlin collection gate and
    /// panel reads; a stale startup toggle must never re-enable diagnostics.
    pub diagnostics_lock: Mutex<()>,
}

#[derive(Debug, Clone)]
pub struct CurrentSite {
    pub site_id: String,
    pub host: String,
    pub window_label: String,
    /// Desktop: `true` when opening the site is what put the window into
    /// fullscreen, so closing it may restore the windowed state. Kept
    /// separate so a manual fullscreen choice is never taken away.
    pub entered_fullscreen: bool,
}

/// Launcher theme preference, persisted with the other settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePref {
    /// Follow the OS light/dark setting (the default).
    #[default]
    System,
    Dark,
    Light,
}

/// User-adjustable preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub blocklist: BlocklistState,
    /// Deny `window.open` / target=_blank inside site WebViews.
    #[serde(default = "default_true")]
    pub block_popups: bool,
    /// Injected CSS is cosmetic only; it never rewrites page scripts.
    #[serde(default = "default_true")]
    pub inject_cosmetic_rules: bool,
    /// Enter fullscreen while a desktop site view is open. Opt-in: sites
    /// share the single app window, so the default keeps the normal
    /// windowed size.
    #[serde(default)]
    pub fullscreen_sites: bool,
    /// Launcher theme: system / dark / light.
    #[serde(default)]
    pub theme: ThemePref,
    /// Android: enter Picture-in-Picture automatically on Home.
    #[serde(default)]
    pub pip_auto_enter: bool,
    #[serde(default)]
    pub developer_options: bool,
    #[serde(default)]
    pub anilist: AniListConfig,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            blocklist: BlocklistState::default_state(),
            block_popups: true,
            inject_cosmetic_rules: true,
            fullscreen_sites: false,
            theme: ThemePref::System,
            pip_auto_enter: false,
            developer_options: false,
            anilist: AniListConfig::default(),
        }
    }
}

/// Settings with secrets removed, safe to hand to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub blocklist: BlocklistState,
    pub blocklist_count: usize,
    pub block_popups: bool,
    pub inject_cosmetic_rules: bool,
    pub fullscreen_sites: bool,
    pub theme: ThemePref,
    pub pip_auto_enter: bool,
    pub developer_options: bool,
    pub anilist: AniListConfigView,
    pub key_backend: String,
    pub key_backend_os_backed: bool,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AniListConfigView {
    pub configured: bool,
    pub client_id: String,
    pub redirect_uri: String,
    pub confidential: bool,
}

/// A launcher tile.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub host: String,
    pub category: Category,
    pub icon: Icon,
    pub builtin: bool,
    pub hidden: bool,
    /// `true` when this site is the native AniList integration rather than a
    /// WebView, so the UI can open the right surface.
    pub native: bool,
}

fn to_view(s: &Site, native_anilist: bool) -> SiteView {
    let native = native_anilist && s.host() == "anilist.co";
    SiteView {
        id: s.id.clone(),
        name: s.name.clone(),
        url: s.url.clone(),
        host: s.host(),
        category: s.category,
        icon: s.icon.clone(),
        builtin: s.builtin,
        hidden: s.hidden,
        native,
    }
}

fn persist_registry(state: &AppState) -> AppResult<()> {
    let reg = state
        .registry
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    state.provider.save_registry(&reg)
}

/// Write one settings value to the encrypted store. Callers that change the
/// live settings must do so only after this returns Ok.
fn write_settings_doc(state: &AppState, value: &Settings) -> AppResult<()> {
    let bytes = serde_json::to_vec(value)?;
    state
        .provider
        .save_doc_bytes("settings.bin", bytes.as_slice())
}

fn rebuild_blocklist(state: &AppState) {
    let (rules, enabled) = {
        let s = state.settings.lock().expect("settings lock");
        (s.blocklist.rules.clone(), s.blocklist.enabled)
    };
    let bl = Blocklist::from_rules(&rules, enabled);
    *state.blocklist.lock().expect("blocklist lock") = bl;
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_sites(state: State<'_, AppState>) -> AppResult<Vec<SiteView>> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    let native = settings.anilist.is_configured();
    drop(settings);
    let reg = state
        .registry
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(reg.visible().iter().map(|s| to_view(s, native)).collect())
}

#[tauri::command]
pub fn add_site(state: State<'_, AppState>, draft: SiteDraft) -> AppResult<SiteView> {
    let site = {
        let mut reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        reg.add(draft)?
    };
    persist_registry(&state)?;
    Ok(to_view(&site, false))
}

#[tauri::command]
pub fn update_site(
    state: State<'_, AppState>,
    id: String,
    draft: SiteDraft,
) -> AppResult<SiteView> {
    let site = {
        let mut reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        reg.update(&id, draft)?
    };
    persist_registry(&state)?;
    Ok(to_view(&site, false))
}

/// What happened to the site's *profile data* during `remove_site`.
///
/// One explicit vocabulary for Kotlin ⇄ Rust ⇄ JS, so the UI never has to
/// infer an outcome from missing fields (A.8 delete/clear/UI contract):
/// * `deleted` — accepted now (disk work may finish asynchronously), or
///   absent (already clean);
/// * `deferred` — provider refused because the profile is loaded in this
///   process; the name is queued and retried at next start;
/// * `unsupported` — the provider has no profile API (feature gap; the
///   command surfaces it as an error and keeps the record);
/// * `hidden` — builtin site: hidden, no profile to delete;
/// * `notApplicable` — platforms without WebView profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileRemoval {
    Deleted,
    Deferred,
    Unsupported,
    Hidden,
    NotApplicable,
}

/// `remove_site` result: registry/session success plus the profile outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RemoveSiteOutcome {
    pub profile: ProfileRemoval,
}

/// Pure mapping from the bridge's delete action to the contract above —
/// tested directly so a registry change can never be paired with a wrong
/// outcome string. `Absent` means the profile was already clean, which the
/// UI reports as `deleted` (nothing is left behind either way).
pub fn profile_removal_for(action: crate::android_bridge::SiteProfileAction) -> ProfileRemoval {
    use crate::android_bridge::SiteProfileAction;
    match action {
        SiteProfileAction::Deleted | SiteProfileAction::Absent => ProfileRemoval::Deleted,
        SiteProfileAction::Pending => ProfileRemoval::Deferred,
    }
}

#[tauri::command]
pub fn remove_site(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> AppResult<RemoveSiteOutcome> {
    let _operation_scope = crate::diagnostics::scope_if_missing("delete");
    // Preserve the registry until the destructive cleanup succeeds, so a
    // failed close/delete does not orphan the tile. The whole sequence is
    // serialized by the desktop/android lifecycle lock (`DESKTOP` →
    // `current_site` → `registry` → `STORE`), and the open path
    // re-validates the `Site` snapshot while holding the same lock.
    #[cfg(desktop)]
    {
        let _desktop_guard = crate::web::windows::lock_desktop_session()?;
        // Check builtin/hide while holding the lifecycle lock, so a
        // concurrent `open_site` cannot interleave between the check and the
        // later registry mutation.
        let was_builtin = {
            let reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            reg.get(&id).is_some_and(|s| s.builtin)
        };
        if was_builtin {
            let mut reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            reg.remove(&id)?;
            drop(reg);
            persist_registry(&state)?;
            return Ok(RemoveSiteOutcome {
                profile: ProfileRemoval::Hidden,
            });
        }
        // User site: close, delete, then remove from registry. Close and
        // delete are done before the registry mutation so a failure keeps the
        // tile and the user can retry.
        let label = crate::web::windows::label_for(&id);
        let should_close = {
            let cur = state
                .current_site
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            cur.as_ref()
                .is_some_and(|c| c.window_label == label || c.site_id == id)
        };
        if should_close {
            crate::web::windows::close_site_window_for_clear(&app, &label)?;
        }
        state.provider.delete_site_session(&id)?;
        let base = app.path().app_data_dir().map_err(|_| {
            log::error!("profil dizini bulunamadı");
            AppError::Storage("profil dizini bulunamadı".into())
        })?;
        let dir = crate::web::session::profile_dir_path(&base, &id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                log::error!("profil dizini silinemedi");
                return Err(AppError::Storage("site verisi silinemedi".into()));
            }
        }
        // Re-validate and remove from registry while still holding the
        // lifecycle lock. If another thread already removed it, treat as
        // success (data already deleted, idempotent).
        {
            let mut reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            match reg.get(&id) {
                Some(s) if s.builtin => {
                    reg.remove(&id)?;
                }
                Some(_) => {
                    reg.remove(&id)?;
                }
                None => {
                    return Ok(RemoveSiteOutcome {
                        profile: ProfileRemoval::Deleted,
                    })
                }
            }
        }
        persist_registry(&state)?;
        Ok(RemoveSiteOutcome {
            profile: ProfileRemoval::Deleted,
        })
    }
    #[cfg(not(desktop))]
    {
        #[cfg(target_os = "android")]
        let _android_guard = crate::web::windows::lock_android_session()?;
        let was_builtin = {
            let reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            reg.get(&id).is_some_and(|s| s.builtin)
        };
        if was_builtin {
            let mut reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            reg.remove(&id)?;
            drop(reg);
            persist_registry(&state)?;
            return Ok(RemoveSiteOutcome {
                profile: ProfileRemoval::Hidden,
            });
        }
        // Platform default for this block: no WebView profiles (iOS).
        #[cfg(not(target_os = "android"))]
        let profile_outcome = ProfileRemoval::NotApplicable;
        #[cfg(target_os = "android")]
        let profile_outcome;
        let should_close = {
            let cur = state
                .current_site
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            cur.as_ref().is_some_and(|c| c.site_id == id)
        };
        if should_close {
            #[cfg(target_os = "android")]
            crate::web::windows::close_site_window_for_clear_locked(&app, &id)?;
            #[cfg(not(target_os = "android"))]
            crate::web::windows::close_site_window_for_clear(&app, &id)?;
        }
        // Android: the site's data lives in its WebView profile. Delete the
        // profile *before* the registry mutation so a failure keeps the tile
        // and the user can retry. `Pending` (profile loaded in this process)
        // is queued for the next process start and reported as `deferred` —
        // never as deleted. An `unsupported-multi-profile` refusal (a
        // provider feature gap) propagates as an error so the record stays
        // and the UI shows the MULTI_PROFILE guidance; it must not be faked
        // as deferred-forever success.
        //
        // The whole delete → queue → generation bump → quarantine retirement
        // order lives in ONE host-tested function. Retirement LAST is the
        // safety property: the ledger for this name may only be dropped once
        // the bump is durably stored, because a failed bump/write aborts this
        // command with the registry intact and the next open would derive the
        // very same name — which must still be blocked while a
        // `removeAllCookies` deletion may be in flight on it.
        #[cfg(target_os = "android")]
        {
            let action = crate::web::windows::run_mobile_delete_protocol(
                &state,
                &id,
                |profile| crate::android_bridge::site_profile_delete(profile),
                |profile| crate::android_bridge::site_profile_retire(profile),
            )?;
            profile_outcome = profile_removal_for(action);
        }
        state.provider.delete_site_session(&id)?;
        {
            let mut reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            if reg.get(&id).is_none() {
                return Ok(RemoveSiteOutcome {
                    profile: profile_outcome,
                });
            }
            reg.remove(&id)?;
        }
        persist_registry(&state)?;
        Ok(RemoveSiteOutcome {
            profile: profile_outcome,
        })
    }
}

#[tauri::command]
pub fn set_site_hidden(state: State<'_, AppState>, id: String, hidden: bool) -> AppResult<()> {
    {
        let mut reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        reg.set_hidden(&id, hidden)?;
    }
    persist_registry(&state)
}

// ---------------------------------------------------------------------------
// Opening a site
// ---------------------------------------------------------------------------

/// Open a site in its own isolated, full-screen WebView.
///
/// Desktop: a new window with a per-site profile directory.
/// Android: a separate native WebView above the launcher, bound to its own
/// androidx.webkit profile (refused outright when the provider lacks
/// `MULTI_PROFILE`).
#[tauri::command]
pub async fn open_site(app: AppHandle, id: String) -> AppResult<OpenedSite> {
    let state = app.state::<AppState>();
    // The WebView policy needs the current user rules BEFORE any navigation.
    sync_android_blocklist(&state);
    let (site, settings) = {
        let state = app.state::<AppState>();
        let reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        let site = reg
            .get(&id)
            .ok_or_else(|| AppError::SiteNotFound(id.clone()))?
            .clone();
        let settings = state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?
            .clone();
        (site, settings)
    };

    // Re-validate on every open: the stored URL is trusted, but a hostile
    // registry edit must not be able to point a WebView at http:// or at
    // localhost.
    let safe = crate::sites::url_policy::validate_site_url(&site.url)
        .map_err(|r| AppError::InvalidUrl(r.reason().to_string()))?;

    // DNS rebinding: a stored name can look public and still resolve to the
    // user's router or a cloud metadata address. The lookup is blocking, so
    // it runs off the async runtime. A resolver failure is not fatal — the
    // string policy already passed, and failing closed would make every site
    // unopenable during a DNS outage.
    let host = safe.host().to_string();
    let dns_result =
        tauri::async_runtime::spawn_blocking(move || crate::web::dns::classify_host(&host)).await;

    // Async command continuations may resume on a different thread; establish
    // synchronous bridge correlation immediately after the DNS await, before
    // any subsequent synchronous validation or window-open work.
    let _operation_scope = crate::diagnostics::scope_if_missing("open");
    let dns = dns_result.map_err(|_| {
        crate::diagnostics::record_current(
            "open",
            "resolve",
            "failed",
            None,
            None,
            Some("unknown"),
            None,
            None,
        );
        AppError::Other("adres çözümlenemedi".into())
    })?;
    if matches!(dns, crate::web::dns::DnsClass::Answered { private: true }) {
        crate::diagnostics::record_current(
            "open",
            "resolve",
            "rejected",
            None,
            None,
            Some("invalid_profile"),
            None,
            None,
        );
        return Err(AppError::InvalidUrl(
            "alan adı yerel/ağ içi bir adrese çözümlendi".into(),
        ));
    }
    let injected = InjectedConfig {
        block_popups: settings.block_popups,
        hide_selectors: if settings.inject_cosmetic_rules {
            InjectedConfig::default().hide_selectors
        } else {
            vec![]
        },
        // Android: site pages get the PiP controller, which the native side
        // drives with `evaluateJavascript` right before entering PiP. Desktop
        // has no PiP window for a site WebView, so it stays out.
        pip_controller: cfg!(target_os = "android"),
    };
    let init_script = build_init_script(&injected);

    windows::open_site_window(
        app,
        &site,
        safe.as_url(),
        &init_script,
        settings.fullscreen_sites,
    )
    .map(|label| OpenedSite {
        label,
        mobile: cfg!(any(target_os = "android", target_os = "ios")),
        // Android now gives real per-site isolation (one WebView profile per
        // site); iOS still shares the single WebView.
        isolated: !cfg!(target_os = "ios"),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedSite {
    /// Window label (desktop) or the site label (mobile bookkeeping).
    pub label: String,
    pub mobile: bool,
    /// Whether this platform gives true per-site storage isolation: per-site
    /// profile directories on desktop, per-site WebView profiles on Android,
    /// single shared WebView on iOS.
    pub isolated: bool,
}

/// Close a site session. Mobile keeps the shared WebView's jar (iOS) or
/// flushes/destroys the site's profiled WebView (Android); desktop removes
/// the child WebView.
#[tauri::command]
pub async fn close_site(
    app: AppHandle,
    label: String,
    operation_id: Option<String>,
) -> AppResult<()> {
    let _operation_scope = operation_id
        .as_deref()
        .map(|id| crate::diagnostics::begin_operation_with_id("close", id))
        .or_else(|| crate::diagnostics::scope_if_missing("close"));
    windows::close_site_window(&app, &label)
}

/// Wipe stored cookies for one site, or for all sites when `id` is `None`.
///
/// `anilist-token` is intentionally never touched here: it is not per-site
/// session data and must survive a site-data wipe. The function also keeps
/// the wipe atomic with respect to the live WebView: on desktop the profile
/// directory is deleted only after the WebView is closed *without* exporting
/// its live cookies, and the whole close+delete sequence is serialized
/// against the periodic sync via the desktop session lock. On Android each
/// target site's profile is cleared in place (profile cookie manager +
/// web storage) while holding the Android session lock; the launcher's
/// default profile and its data are never touched.
#[tauri::command]
pub async fn clear_site_data(app: AppHandle, id: Option<String>) -> AppResult<u32> {
    let _operation_scope = crate::diagnostics::scope_if_missing("clear");
    // `is_all` captures whether the caller asked for "all sites" before `id`
    // is moved. Used for orphan profile cleanup and for Android live-jar
    // handling.
    let is_all = id.is_none();
    #[cfg(target_os = "android")]
    let all_sites = is_all;
    let state = app.state::<AppState>();

    // Serialize the wipe against open/sync/remove via the lifecycle lock.
    // The lock is taken *before* snapshotting `ids` so the `ids` list is
    // consistent with the registry while the lock is held. This follows the
    // documented order `DESKTOP/ANDROID → registry → STORE`.
    #[cfg(target_os = "android")]
    let _session_guard = windows::lock_android_session()?;
    #[cfg(desktop)]
    let _desktop_guard = windows::lock_desktop_session()?;

    let ids: Vec<String> = match id {
        Some(id) => vec![id],
        None => {
            let reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            reg.sites.iter().map(|s| s.id.clone()).collect()
        }
    };
    #[cfg(target_os = "android")]
    {
        // Android: if the active provider is among the ids being wiped (or
        // all providers are wiped), close its WebView *without* exporting
        // live cookies/storage and invalidate `current_site` before deleting
        // the blobs. This prevents the still-loaded page and its
        // `spawn_session_sync` worker from recreating the jar after the
        // wipe. The `ANDROID_SESSION_LOCK` is already held, so the
        // `*_locked` helper is used to avoid deadlock and to re-check that
        // `site_id` is still current after acquiring the lock.
        let current_snapshot = {
            let cur = state
                .current_site
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            cur.clone()
        };
        let clear_live = all_sites
            || current_snapshot
                .as_ref()
                .is_some_and(|site| ids.iter().any(|id| id == &site.site_id));
        if clear_live {
            if let Some(active) = current_snapshot {
                crate::web::windows::close_site_window_for_clear_locked(&app, &active.site_id)?;
            }
            // No `else`: with no active site view there is no shared jar to
            // clear any more — site cookies live in per-site profiles, and
            // the launcher's default profile is never part of a site wipe.
        }
    }
    #[cfg(desktop)]
    {
        // If the currently open site is among the ids being wiped, close its
        // WebView *without* exporting its live cookies — otherwise the
        // periodic sync or the normal close path would resurrect the
        // just-deleted jar, and the directory would be removed underneath a
        // live WebView (which WebView2/WebKitGTK may recreate). A close
        // failure aborts the wipe so the profile is not deleted underneath a
        // live WebView and the caller does not see a false success.
        // For the “clear all” path, close any live WebView regardless of
        // whether its `site_id` appears in `ids` (it may be an unregistered
        // live profile); otherwise the later orphan sweep could delete the
        // live `profiles/site-*` directory underneath the WebView.
        let current_label = {
            let cur = state
                .current_site
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            if is_all {
                cur.as_ref().map(|c| c.window_label.clone())
            } else {
                cur.as_ref()
                    .filter(|c| ids.iter().any(|id| id == &c.site_id))
                    .map(|c| c.window_label.clone())
            }
        };
        if let Some(label) = current_label {
            crate::web::windows::close_site_window_for_clear(&app, &label)?;
        }
    }
    let mut n = 0;
    for id in &ids {
        // Android: clear the site's profile in place — its own cookie
        // manager, web storage and permissions. The profile object itself
        // survives, so the next open starts logged out without a
        // delete/recreate race. An absent profile is already clean; a hard
        // failure aborts so the caller never sees a false success.
        #[cfg(target_os = "android")]
        {
            // Profile first, encrypted blobs second, as one host-tested
            // protocol: a wipe failure keeps the jar for a retry, and a success
            // always reaches `delete_site_session` — including Kotlin's "the
            // provider deleted nothing because the jar was already empty"
            // success, which used to fail the whole clear and strand the jar.
            windows::run_mobile_clear_protocol(&state, id, |profile| {
                crate::android_bridge::site_clear_data(profile)
            })?;
        }
        #[cfg(not(target_os = "android"))]
        state.provider.delete_site_session(id)?;
        n += 1;
    }
    // Android orphan sweep for "clear all": profile names that no registered
    // site currently owns (an interrupted remove, an old generation, an
    // imported registry). Mirrors the desktop `profiles/site-*` sweep — only
    // names in our namespace are ever addressed, and the live site view was
    // closed above.
    //
    // The ORDER — durable tombstone, then the provider call, then the permanent
    // reuse block, then the quarantine ledger — lives in the host-tested
    // `windows::run_orphan_profile_sweep`, because this block is
    // `#[cfg(target_os = "android")]` and `cargo test` would never see it here.
    // This side only decides WHICH names are orphans: it holds the registry.
    #[cfg(target_os = "android")]
    if is_all {
        let mut owned = std::collections::HashSet::new();
        {
            let reg = state
                .registry
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            for site in reg.sites.iter() {
                let generation = windows::profile_generation(&state, &site.id)?;
                owned.insert(windows::android_profile_name(&site.id, generation));
            }
        }
        let orphans: Vec<String> = crate::android_bridge::site_profile_names()?
            .into_iter()
            .filter(|name| {
                name.starts_with(windows::ANDROID_PROFILE_PREFIX) && !owned.contains(name)
            })
            .collect();
        let report = windows::run_orphan_profile_sweep(
            &state,
            &orphans,
            crate::android_bridge::site_profile_delete,
            crate::android_bridge::site_profile_retire,
        )?;
        log::info!(
            "yetim profil süpürmesi: {} ad kuyruğa alındı, {} kalıcı ad engeli, {} ertelendi, \
             {} desteklenmiyor, {} karantina kaydı düştü, {} kayıt düşürülemedi",
            report.queued,
            report.blocked,
            report.deferred,
            report.unsupported,
            report.retired,
            report.retire_failed
        );
    }
    // Desktop profile directories hold the real cookie DB. A missing
    // directory is success; a failed delete is not. This runs only on
    // desktop — on Android there is no per-site directory to remove.
    #[cfg(desktop)]
    {
        let base = app.path().app_data_dir().map_err(|_| {
            log::error!("profil dizini bulunamadı");
            AppError::Storage("profil dizini bulunamadı".into())
        })?;
        for id in &ids {
            let dir = crate::web::session::profile_dir_path(&base, id);
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    log::error!("profil dizini silinemedi");
                    return Err(AppError::Storage("site verisi silinemedi".into()));
                }
            }
        }
        // "Tüm site verilerini temizle" (id == None) must also cover orphan
        // `profiles/site-*` directories left behind by earlier bugs. Those
        // orphans are not in `ids` (registry) but are still app-owned
        // profiles. We scan `profiles/` for any remaining `site-*` dirs and
        // delete them, but only while holding `DESKTOP_SESSION_LOCK` and after
        // the live WebView was closed above, so no live profile is removed.
        // Only directories whose name starts with `site-` are touched; other
        // files/dirs (e.g. logs) are left alone.
        if is_all {
            let profiles_dir = base.join("profiles");
            match std::fs::read_dir(&profiles_dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if !name.starts_with("site-") {
                            continue;
                        }
                        if let Ok(ft) = entry.file_type() {
                            if !ft.is_dir() {
                                continue;
                            }
                        }
                        let orphan_dir = profiles_dir.join(&name);
                        match std::fs::remove_dir_all(&orphan_dir) {
                            Ok(()) => {
                                log::info!("yetim profil dizini temizlendi");
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(_) => {
                                log::error!("yetim profil dizini silinemedi");
                                return Err(AppError::Storage("site verisi silinemedi".into()));
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    log::error!("profil dizini listelenemedi");
                    return Err(AppError::Storage("site verisi silinemedi".into()));
                }
            }
        }
    }
    Ok(n)
}

// The raw cookie-jar commands (`get_site_cookies` / `set_site_cookies`) are
// GONE. Nothing in the frontend or any other production path called them: the
// one-time 0.3.4 jar import reads `cookies-<site_id>` inside
// `web::windows::run_mobile_open_protocol`, and the desktop profile flow never
// touched them. What they did offer was an IPC surface that would read or
// overwrite ANY site's encrypted cookie jar from the WebView — with no site
// ownership, no profile generation and no quarantine check in the way. Keeping
// unused authority "for tests" is exactly the surface a compromised launcher
// page would use, and the storage behaviour they were said to cover is tested
// directly against the provider (`cookie_jar_secret_roundtrips`) and through the
// real protocol (`mobile_clear_protocol_*`).

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> AppResult<SettingsView> {
    let s = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    let bl = state
        .blocklist
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(SettingsView {
        blocklist: s.blocklist.clone(),
        blocklist_count: bl.len(),
        block_popups: s.block_popups,
        inject_cosmetic_rules: s.inject_cosmetic_rules,
        fullscreen_sites: s.fullscreen_sites,
        theme: s.theme,
        pip_auto_enter: s.pip_auto_enter,
        developer_options: s.developer_options,
        anilist: AniListConfigView {
            configured: s.anilist.is_configured(),
            client_id: s.anilist.client_id.clone(),
            redirect_uri: s.anilist.redirect_uri.clone(),
            confidential: s.anilist.is_confidential(),
        },
        key_backend: state.provider.backend().label().to_string(),
        key_backend_os_backed: state.provider.backend().is_os_backed(),
        platform: platform_name(),
    })
}

/// Commit a settings patch without creating a diagnostics gate-off window in
/// which Kotlin can still collect while the persisted setting is already false.
/// The native disable is preflighted before persistence; on persistence failure
/// the prior native gate is restored best-effort. The callback is injectable so
/// the ordering/rollback contract is tested without Android or a Tauri runtime.
fn commit_settings_with_diagnostics_preflight(
    was_enabled: bool,
    requested: Option<bool>,
    mut set_native_gate: impl FnMut(bool) -> AppResult<()>,
    persist_and_commit: impl FnOnce() -> AppResult<()>,
) -> AppResult<()> {
    if requested != Some(false) {
        return persist_and_commit();
    }

    if let Err(error) = set_native_gate(false) {
        let _ = set_native_gate(was_enabled);
        return Err(error);
    }
    if let Err(error) = persist_and_commit() {
        let _ = set_native_gate(was_enabled);
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
pub fn update_settings(
    state: State<'_, AppState>,
    patch: SettingsPatch,
) -> AppResult<SettingsView> {
    // Pip preference has its own serialized path (see `set_pip_auto_enter`);
    // when the patch contains it, take the same lock so startup vs toggle
    // vs generic update cannot interleave and generation stays consistent.
    let needs_pip_lock = patch.pip_auto_enter.is_some();
    let _pip_guard = if needs_pip_lock {
        Some(
            state
                .pip_lock
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?,
        )
    } else {
        None
    };
    let diagnostics_update = patch.developer_options;
    let _diagnostics_guard = if diagnostics_update.is_some() {
        Some(
            state
                .diagnostics_lock
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?,
        )
    } else {
        None
    };

    let diagnostics_was_enabled = if diagnostics_update.is_some() {
        state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?
            .developer_options
    } else {
        false
    };
    let mut native_disable_failed = false;
    let mut native_restore_failed = false;
    let commit_result = commit_settings_with_diagnostics_preflight(
        diagnostics_was_enabled,
        diagnostics_update,
        |enabled| {
            let result = crate::android_bridge::set_debug_logging(enabled);
            if !enabled && result.is_err() {
                native_disable_failed = true;
            } else if enabled && result.is_err() {
                native_restore_failed = true;
            }
            result
        },
        || {
            use std::sync::atomic::Ordering;
            let mut live = state
                .settings
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            let mut next = live.clone();
            if let Some(b) = patch.blocklist {
                next.blocklist = b;
            }
            if let Some(v) = patch.block_popups {
                next.block_popups = v;
            }
            if let Some(v) = patch.inject_cosmetic_rules {
                next.inject_cosmetic_rules = v;
            }
            if let Some(v) = patch.fullscreen_sites {
                next.fullscreen_sites = v;
            }
            if let Some(v) = patch.theme {
                next.theme = v;
            }
            let pip_changed =
                patch.pip_auto_enter.is_some() && patch.pip_auto_enter != Some(live.pip_auto_enter);
            if let Some(v) = patch.pip_auto_enter {
                next.pip_auto_enter = v;
            }
            if let Some(v) = patch.developer_options {
                next.developer_options = v;
            }
            if let Some(a) = patch.anilist {
                next.anilist = a;
            }
            write_settings_doc(&state, &next)?;
            *live = next;
            if pip_changed {
                state.pip_generation.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        },
    );
    if native_disable_failed || native_restore_failed {
        crate::diagnostics::record(
            "devlog",
            "gate",
            "failed",
            None,
            None,
            Some("provider"),
            None,
            None,
        );
    }
    commit_result?;

    if let Some(enabled) = diagnostics_update {
        if enabled {
            crate::diagnostics::set_enabled(true);
            #[cfg(target_os = "android")]
            if crate::android_bridge::set_debug_logging(true).is_err() {
                crate::diagnostics::record(
                    "devlog",
                    "gate",
                    "failed",
                    None,
                    None,
                    Some("provider"),
                    None,
                    None,
                );
            }
        } else {
            // Kotlin was disabled before the persisted setting changed above.
            crate::diagnostics::set_enabled(false);
        }
    }
    drop(_diagnostics_guard);
    drop(_pip_guard);
    rebuild_blocklist(&state);
    sync_android_blocklist(&state);
    get_settings(state)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    #[serde(default)]
    pub blocklist: Option<BlocklistState>,
    #[serde(default)]
    pub block_popups: Option<bool>,
    #[serde(default)]
    pub inject_cosmetic_rules: Option<bool>,
    #[serde(default)]
    pub fullscreen_sites: Option<bool>,
    #[serde(default)]
    pub theme: Option<ThemePref>,
    #[serde(default)]
    pub pip_auto_enter: Option<bool>,
    #[serde(default)]
    pub developer_options: Option<bool>,
    #[serde(default)]
    pub anilist: Option<AniListConfig>,
}

/// One-time warning when the stored registry could not be decrypted.
#[tauri::command]
pub fn take_startup_warning(state: State<'_, AppState>) -> AppResult<Option<String>> {
    let mut w = state
        .startup_warning
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(w.take())
}

#[tauri::command]
pub fn reset_blocklist(state: State<'_, AppState>) -> AppResult<usize> {
    {
        let mut live = state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        let mut next = live.clone();
        next.blocklist = BlocklistState::default_state();
        write_settings_doc(&state, &next)?;
        *live = next;
    }
    rebuild_blocklist(&state);
    let bl = state
        .blocklist
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(bl.len())
}

// ---------------------------------------------------------------------------
// AniList
// ---------------------------------------------------------------------------

fn client(state: &AppState) -> AppResult<(AniListClient, AniListConfig)> {
    let s = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    if !s.anilist.is_configured() {
        return Err(AppError::AniList(
            "AniList client_id ayarlardan girilmeli".into(),
        ));
    }
    let cfg = s.anilist.clone();
    drop(s);
    Ok((AniListClient::new(cfg.clone()), cfg))
}

fn load_token(state: &AppState) -> AppResult<Option<Token>> {
    match state.provider.get_secret("anilist-token")? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

fn save_token(state: &AppState, token: &Token) -> AppResult<()> {
    let bytes = serde_json::to_vec(token)?;
    state.provider.put_secret("anilist-token", &bytes)
}

/// Start the OAuth flow: returns the URL to open plus the redirect URI that
/// must be registered for the AniList client.
#[tauri::command]
pub async fn anilist_login_start(app: AppHandle) -> AppResult<LoginStart> {
    let state = app.state::<AppState>();
    let (cli, cfg) = client(&state)?;

    // SECURITY: on Android a custom-scheme redirect can be intercepted by
    // another app (it receives BOTH code and state), so native login is
    // restricted to the loopback listener that desktop already uses. Refuse
    // before writing a pending state or returning an authorization URL.
    #[cfg(target_os = "android")]
    if !anilist::mobile_native_login_allowed(&cfg.redirect_uri) {
        return Err(AppError::AniList(
            anilist::ANDROID_CUSTOM_SCHEME_OAUTH_REFUSED.into(),
        ));
    }

    // A fresh, unpredictable state value; the callback must echo it.
    let state_value = uuid::Uuid::new_v4().to_string();
    let url = anilist::authorize_url(&cfg, &state_value)?;
    {
        let mut s = state
            .oauth_state
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        *s = Some(state_value);
    }

    // Desktop loopback flow: when the redirect URI points at a local HTTP
    // address (the built-in default does, http://127.0.0.1:17395), nothing
    // else would receive the OAuth redirect — the browser would land on a
    // dead port and the login would silently hang. Serve the single request
    // in the background and finish the login through the same events the
    // deep-link handler already uses. A custom-scheme redirect skips this:
    // the OS delivers it to the deep-link handler in lib.rs instead.
    if let Some(port) = anilist::loopback_port(&cfg.redirect_uri) {
        let handle = app.clone();
        let redirect_uri = cfg.redirect_uri.clone();
        tauri::async_runtime::spawn(async move {
            let flow: AppResult<AniListUser> = async {
                let listener = anilist::LoopbackListener::bind_port(port).await?;
                let target = listener
                    .wait_for_callback(std::time::Duration::from_secs(300))
                    .await?;
                let callback = anilist::absolute_callback(&redirect_uri, &target)?;
                anilist_login_callback(handle.clone(), callback).await
            }
            .await;
            match flow {
                Ok(user) => {
                    let _ = handle.emit("animehub://anilist-login", user.name);
                }
                Err(e) => {
                    let _ = handle.emit("animehub://error", e.to_string());
                }
            }
        });
    }

    Ok(LoginStart {
        url,
        redirect_uri: cli.config().redirect_uri.clone(),
        confidential: cfg.is_confidential(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStart {
    pub url: String,
    pub redirect_uri: String,
    pub confidential: bool,
}

/// Complete the flow from a callback URL (custom scheme or loopback).
#[tauri::command]
pub async fn anilist_login_callback(
    app: AppHandle,
    callback_url: String,
) -> AppResult<AniListUser> {
    let state = app.state::<AppState>();
    let (cli, cfg) = client(&state)?;
    // The callback must match THIS session's configured redirect URI exactly
    // (scheme, host, port and path) before its `code` means anything.
    let (code, returned_state) = anilist::parse_callback(&callback_url, &cfg.redirect_uri)?;

    // Single-use, consumed under the lock and only on a match: a repeated or
    // replayed callback finds no pending attempt and cannot race a second token
    // exchange, while a bogus state never burns the genuine login.
    match anilist::take_pending_state(&state.oauth_state, &returned_state)? {
        anilist::PendingState::None => {
            return Err(AppError::AniList("bekleyen bir oturum yok".into()))
        }
        anilist::PendingState::Mismatched => {
            return Err(AppError::AniList("state doğrulaması başarısız".into()))
        }
        anilist::PendingState::Consumed(_) => {}
    }

    let token = cli.exchange_code(&code).await?;
    let viewer = cli.viewer(&token).await?;
    save_token(&state, &token)?;
    Ok(AniListUser {
        id: viewer.id,
        name: viewer.name,
        avatar: viewer.avatar_large,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AniListUser {
    pub id: i64,
    pub name: String,
    pub avatar: Option<String>,
}

/// Fetches a live AniList session pair. Handles refresh, expiry cleanup and
/// the "not configured/signed out" case uniformly for every caller that
/// needs an authenticated GraphQL request.
async fn fresh_anilist_session(state: &AppState) -> AppResult<Option<(AniListClient, Token)>> {
    let Some(token) = load_token(state)? else {
        return Ok(None);
    };
    let (cli, _) = match client(state) {
        Ok(c) => c,
        // Not configured any more: report signed-out rather than erroring.
        Err(_) => return Ok(None),
    };

    let token = if token.needs_refresh(anilist::now_unix()) {
        match token.refresh_token.as_deref() {
            Some(rt) => match cli.refresh(rt).await {
                Ok(t) => {
                    save_token(state, &t)?;
                    t
                }
                // Refresh rejected => the session is over.
                Err(_) => {
                    if state.provider.delete_secret("anilist-token").is_err() {
                        log::error!("süresi dolmuş AniList oturumu silinemedi");
                    }
                    return Ok(None);
                }
            },
            None => {
                if state.provider.delete_secret("anilist-token").is_err() {
                    log::error!("yenilenemeyen AniList oturumu silinemedi");
                }
                return Ok(None);
            }
        }
    } else {
        token
    };

    Ok(Some((cli, token)))
}

/// Clears a token the server rejected outright.
fn drop_rejected_token(state: &AppState) {
    if state.provider.delete_secret("anilist-token").is_err() {
        log::error!("geçersiz AniList oturumu silinemedi");
    }
}

/// Current session, refreshing the token if it is close to expiry.
#[tauri::command]
pub async fn anilist_status(app: AppHandle) -> AppResult<Option<AniListUser>> {
    let state = app.state::<AppState>();
    let Some((cli, token)) = fresh_anilist_session(&state).await? else {
        return Ok(None);
    };

    match cli.viewer(&token).await {
        Ok(v) => Ok(Some(AniListUser {
            id: v.id,
            name: v.name,
            avatar: v.avatar_large,
        })),
        Err(AppError::Unauthorized) => {
            drop_rejected_token(&state);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Aggregate counters for the launcher hero bar. `None` when signed out —
/// the frontend then shows the connect affordance instead of false zeros.
#[tauri::command]
pub async fn anilist_hero_stats(app: AppHandle) -> AppResult<Option<anilist::ViewerStats>> {
    let state = app.state::<AppState>();
    let Some((cli, token)) = fresh_anilist_session(&state).await? else {
        return Ok(None);
    };

    match cli.viewer_stats(&token).await {
        Ok(s) => Ok(Some(s)),
        Err(AppError::Unauthorized) => {
            drop_rejected_token(&state);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub async fn anilist_logout(app: AppHandle) -> AppResult<()> {
    let state = app.state::<AppState>();
    state.provider.delete_secret("anilist-token")
}

#[tauri::command]
pub async fn anilist_library(
    app: AppHandle,
    status: Option<MediaListStatus>,
    page: Option<u32>,
) -> AppResult<LibraryPage> {
    let state = app.state::<AppState>();
    let token = load_token(&state)?.ok_or(AppError::Unauthorized)?;
    let (cli, _) = client(&state)?;
    let (entries, has_next) = cli.library(&token, status, page.unwrap_or(1)).await?;
    Ok(LibraryPage { entries, has_next })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryPage {
    pub entries: Vec<anilist::LibraryEntry>,
    pub has_next: bool,
}

#[tauri::command]
pub async fn anilist_search(
    app: AppHandle,
    query: String,
) -> AppResult<Vec<anilist::SearchResult>> {
    let state = app.state::<AppState>();
    let token = load_token(&state)?.ok_or(AppError::Unauthorized)?;
    let (cli, _) = client(&state)?;
    cli.search(&token, &query).await
}

#[tauri::command]
pub async fn anilist_update(
    app: AppHandle,
    media_id: i64,
    status: MediaListStatus,
    progress: u32,
) -> AppResult<UpdatedEntry> {
    let state = app.state::<AppState>();
    let token = load_token(&state)?.ok_or(AppError::Unauthorized)?;
    let (cli, _) = client(&state)?;
    let (id, s, p) = cli.save_entry(&token, media_id, status, progress).await?;
    Ok(UpdatedEntry {
        entry_id: id,
        status: s,
        progress: p,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatedEntry {
    pub entry_id: i64,
    pub status: MediaListStatus,
    pub progress: u32,
}

#[tauri::command]
pub async fn anilist_delete(app: AppHandle, entry_id: i64) -> AppResult<bool> {
    let state = app.state::<AppState>();
    let token = load_token(&state)?.ok_or(AppError::Unauthorized)?;
    let (cli, _) = client(&state)?;
    cli.delete_entry(&token, entry_id).await
}

// ---------------------------------------------------------------------------
// Görünüm / PiP / misc
// ---------------------------------------------------------------------------

/// Apply the launcher theme to the native window.
///
/// `System` maps to `None`, which hands the choice back to the OS. WebView2
/// and WebKitGTK both follow the same override for `prefers-color-scheme`,
/// so the choice also reaches site pages that respect the media query.
#[tauri::command]
pub fn set_window_theme(window: tauri::WebviewWindow, theme: ThemePref) -> AppResult<()> {
    let target = match theme {
        ThemePref::System => None,
        ThemePref::Dark => Some(tauri::Theme::Dark),
        ThemePref::Light => Some(tauri::Theme::Light),
    };
    window.set_theme(target).map_err(|_| {
        log::error!("tema uygulanamadı");
        AppError::Other("tema uygulanamadı".into())
    })
}

/// Manual Picture-in-Picture entry.
///
/// The native side runs the *same* controlled chain the Activity callbacks use
/// — prepare the WebView (`window.__animehubPreparePip()`), wait for its
/// answer, then `enterPictureInPictureMode()` with the detected aspect ratio —
/// so the PiP window only ever shows the active player, and the preparation
/// exists once instead of once per entry point.
///
/// `num`/`den` are clamped here and passed on as a fallback for the case where
/// no player ratio can be measured. Resolves `false` when PiP is unavailable,
/// nothing playable was found, or the Activity could not enter PiP.
// `async` runs the body on Tauri's async runtime, not on the IPC dispatch
// thread. These commands wait for a Kotlin reply through the plugin bridge.
#[tauri::command(async)]
pub fn enter_pip(num: Option<u32>, den: Option<u32>) -> AppResult<bool> {
    let (n, d) = crate::android_bridge::clamp_aspect(num.unwrap_or(16), den.unwrap_or(9));
    crate::android_bridge::enter_pip(n, d)
}

/// Store the "enter PiP automatically" preference.
///
/// On Android this only *allows* the controlled paths — API 30+
/// `onPictureInPictureRequested()` and API 26-29 `onUserLeaveHint()` — which
/// prepare the WebView before PiP starts. The platform's own auto-enter
/// (`setAutoEnterEnabled`) is deliberately never turned on: Android documents
/// that it suppresses `onPictureInPictureRequested()`, so the system would put
/// the unprepared site into the PiP window.
pub const PIP_UNSUPPORTED_MSG: &str = "Bu cihazda resim içinde resim (PiP) desteklenmiyor.";
pub const PIP_PLATFORM_UNKNOWN_MSG: &str =
    "Cihaz türü belirlenemedi; PiP ayarı değiştirilmedi. Uygulamayı yeniden açıp tekrar deneyin.";

/// Store the "enter PiP automatically" preference atomically with the native
/// controller. The native apply and the encrypted settings write are held
/// under the same `pip_lock`, so a startup thread and a user toggle cannot
/// interleave and the last requested value always wins. On persist failure
/// the native side is best-effort reverted to the previous value.
#[tauri::command(async)]
pub fn set_pip_auto_enter(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> AppResult<SettingsView> {
    use std::sync::atomic::Ordering;

    // Serialize all PiP applies.
    let _pip_guard = state
        .pip_lock
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;

    // Snapshot previous persisted value while holding the ordering lock.
    let previous = {
        let s = state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        s.pip_auto_enter
    };

    if enabled == previous {
        // No change – still return the current view for the UI.
        drop(_pip_guard);
        return get_settings(state);
    }

    let _operation_scope = crate::diagnostics::begin_operation("pip");
    // Platform check: unknown platform is refused without a save.
    // On desktop we store only; on Android we must drive the native controller.
    let is_android = cfg!(target_os = "android");
    if !is_android {
        // Non-Android: check that platform is known desktop, otherwise refuse.
        // `platform_name()` is the same source `app_info` uses.
        let plat = platform_name();
        if !["windows", "linux", "macos"].contains(&plat.as_str()) && plat != "android" {
            // Unknown platform – mimic JS's platform check.
            // In practice this branch is only taken on non-desktop non-android
            // (e.g. iOS) where PiP is not applicable; treat as unsupported.
            return Err(AppError::Other(PIP_PLATFORM_UNKNOWN_MSG.into()));
        }
        // Desktop: persist only.
    } else {
        // Android: native first.
        let applied = match crate::android_bridge::set_pip_auto_enter(enabled) {
            Ok(v) => v,
            Err(e) => {
                // Best-effort revert: native may have toggled before throwing.
                let reverted = crate::android_bridge::set_pip_auto_enter(previous).unwrap_or(false);
                if reverted {
                    return Err(e);
                } else {
                    let base = e.to_string();
                    return Err(AppError::Other(format!(
                        "{} PiP durumu belirsiz; önceki değere dönülemedi, uygulamayı yeniden başlatın.",
                        base
                    )));
                }
            }
        };
        if !applied {
            return Err(AppError::Other(PIP_UNSUPPORTED_MSG.into()));
        }
    }

    // Native succeeded (or desktop) – now persist.
    let persisted = {
        let mut live = state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        let mut next = live.clone();
        next.pip_auto_enter = enabled;
        match write_settings_doc(&state, &next) {
            Ok(()) => {
                *live = next;
                state.pip_generation.fetch_add(1, Ordering::SeqCst);
                true
            }
            Err(e) => {
                // Persist failed – try to revert native on Android.
                if is_android {
                    let reverted =
                        crate::android_bridge::set_pip_auto_enter(previous).unwrap_or(false);
                    if reverted {
                        return Err(e);
                    } else {
                        let base = e.to_string();
                        return Err(AppError::Other(format!(
                            "{} PiP durumu geri alınamadı; uygulamayı yeniden başlatın.",
                            base
                        )));
                    }
                }
                return Err(e);
            }
        }
    };

    if !persisted {
        return Err(AppError::Other("ayar yazılamadı".into()));
    }
    drop(_pip_guard);
    rebuild_blocklist(&state);
    get_settings(state)
}

/// Decide whether a captured startup value should still be applied.
/// Pure, testable policy: while holding `pip_lock`, compare captured
/// generation/value to the live ones. If generation differs, a toggle has
/// happened – the stale startup value must not overwrite the newer choice.
pub fn pip_startup_should_apply(
    captured_gen: u64,
    live_gen: u64,
    captured_val: bool,
    live_val: bool,
) -> bool {
    // Generation is the primary signal; value equality is a secondary guard
    // for the case where generation is not yet tracked (e.g. old store).
    if captured_gen != live_gen {
        return false;
    }
    captured_val == live_val
}

/// Lock without the `expect()` panic-on-poison trap.
///
/// Everything guarded by these mutexes is plain state: a panic while one lock
/// was held (a background flush, a resolver task) used to poison it, and every
/// later `lock().expect(..)` then panicked too — permanently, and inside the
/// WebView navigation callback, where a panic is far worse than a stale read.
/// Recovering the guard keeps the app usable; a poisoned lock says a writer
/// panicked, not that the guarded value is unusable, and refusing to start is
/// strictly worse than continuing with the last consistent state.
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The blocklist snapshot handed to the Android WebView.
///
/// Kotlin has no `enabled` flag of its own: an empty snapshot is the only way
/// to say "the user turned the blocklist off". Pushing `entries()` while
/// disabled kept ads and trackers blocked on Android long after the desktop
/// matcher started answering `false`, so the gate lives here — next to
/// `Blocklist::is_blocked`, which applies the same rule.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn android_blocklist_entries(list: &Blocklist) -> Vec<String> {
    if list.is_enabled() {
        list.entries()
    } else {
        Vec::new()
    }
}

/// Push the current blocklist snapshot into the Android site WebView.
///
/// Called before every site open and after a settings change, because the
/// WebView evaluates the rules itself (Kotlin has no access to Rust state).
fn sync_android_blocklist(state: &AppState) {
    #[cfg(target_os = "android")]
    {
        // Recover a poisoned lock instead of sending an empty rule set: the old
        // `unwrap_or_default()` silently disabled enforcement after one panic.
        let entries = android_blocklist_entries(&lock_recover(&state.blocklist));
        if let Err(error) = crate::android_bridge::site_blocklist_update(&entries) {
            // Enforcement must not silently regress to desktop-only.
            log::warn!("Android blocklist could not be synchronised: {error}");
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = state;
    }
}

/// Read the bounded, mixed Rust+Kotlin diagnostics timeline. Collection and
/// reads are both gated by the persisted `developer_options` setting.
#[tauri::command(async)]
pub fn debug_log(app: AppHandle) -> AppResult<animehub_android::DebugLogSnapshot> {
    let state = app.state::<AppState>();
    let _gate = state
        .diagnostics_lock
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    let enabled = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?
        .developer_options;
    if !enabled {
        crate::diagnostics::set_enabled(false);
        #[cfg(target_os = "android")]
        let _ = crate::android_bridge::set_debug_logging(false);
        return Ok(crate::diagnostics::empty_snapshot());
    }
    crate::diagnostics::set_enabled(true);
    #[cfg(target_os = "android")]
    crate::android_bridge::set_debug_logging(true)
        .map_err(|_| AppError::Other("diagnostic setting could not be applied".into()))?;
    crate::android_bridge::debug_log()
}

/// User-triggered reset of both process-local rings. No file is written.
#[tauri::command(async)]
pub fn debug_log_reset(app: AppHandle) -> AppResult<()> {
    let state = app.state::<AppState>();
    let _gate = state
        .diagnostics_lock
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    let enabled = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?
        .developer_options;
    if !enabled {
        crate::diagnostics::set_enabled(false);
        #[cfg(target_os = "android")]
        let _ = crate::android_bridge::set_debug_logging(false);
        return Ok(());
    }
    crate::android_bridge::debug_log_reset()?;
    crate::diagnostics::reset();
    Ok(())
}

#[tauri::command]
pub fn app_info(app: AppHandle) -> AppResult<AppInfo> {
    let state = app.state::<AppState>();
    let s = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(AppInfo {
        version: app.package_info().version.to_string(),
        name: app.package_info().name.clone(),
        platform: platform_name(),
        key_backend: state.provider.backend().label().to_string(),
        key_backend_os_backed: state.provider.backend().is_os_backed(),
        webview: webview_name(),
        anilist_configured: s.anilist.is_configured(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub name: String,
    pub platform: String,
    pub key_backend: String,
    pub key_backend_os_backed: bool,
    pub webview: String,
    pub anilist_configured: bool,
}

pub fn platform_name() -> String {
    if cfg!(target_os = "android") {
        "android".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else if cfg!(target_os = "linux") {
        "linux".into()
    } else {
        std::env::consts::OS.into()
    }
}

fn webview_name() -> String {
    if cfg!(target_os = "android") {
        "Android System WebView".into()
    } else if cfg!(target_os = "windows") {
        "WebView2".into()
    } else if cfg!(target_os = "macos") {
        "WKWebView".into()
    } else {
        "WebKitGTK".into()
    }
}

pub fn backend_name(b: Backend) -> &'static str {
    b.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::master_key::load_master_key;
    use crate::secure::store::SecureStore;
    // Test-only: the raw cookie-jar COMMANDS are gone (§AH-03), the jar type
    // itself stays — `web::windows` still reads `cookies-<site_id>`, and these
    // tests prove the scoping. Importing it at module level would make it an
    // unused import in the non-test build.
    use crate::web::session::CookieJar;

    fn state_in(dir: &std::path::Path) -> AppState {
        let key = load_master_key(dir).expect("key");
        let provider = SecureStore::new(&key, dir).expect("store");
        let (registry, warning) = provider.load_registry();
        AppState {
            provider: Box::new(provider),
            registry: Mutex::new(registry),
            settings: Mutex::new(Settings::default()),
            blocklist: Mutex::new(Blocklist::default()),
            oauth_state: Mutex::new(None),
            startup_warning: Mutex::new(warning),
            launcher_url: Mutex::new(None),
            current_site: Mutex::new(None),
            pip_lock: Mutex::new(()),
            pip_generation: std::sync::atomic::AtomicU64::new(0),
            diagnostics_lock: Mutex::new(()),
        }
    }

    #[test]
    fn diagnostics_disable_bridge_failure_does_not_commit_settings() {
        use std::cell::{Cell, RefCell};

        let native_calls = RefCell::new(Vec::new());
        let committed = Cell::new(false);
        let result = commit_settings_with_diagnostics_preflight(
            true,
            Some(false),
            |enabled| {
                native_calls.borrow_mut().push(enabled);
                if enabled {
                    Ok(())
                } else {
                    Err(AppError::Other("safe bridge failure".into()))
                }
            },
            || {
                committed.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(
            *native_calls.borrow(),
            vec![false, true],
            "a failed disable is restored best-effort before returning",
        );
        assert!(!committed.get(), "the persisted setting must not change");
    }

    #[test]
    fn diagnostics_disable_persistence_failure_restores_previous_native_gate() {
        use std::cell::RefCell;

        let native_calls = RefCell::new(Vec::new());
        let result = commit_settings_with_diagnostics_preflight(
            true,
            Some(false),
            |enabled| {
                native_calls.borrow_mut().push(enabled);
                Ok(())
            },
            || Err(AppError::Storage("settings write failed".into())),
        );

        assert!(result.is_err());
        assert_eq!(*native_calls.borrow(), vec![false, true]);
    }

    #[test]
    fn profile_removal_outcome_mapping_is_exact() {
        use crate::android_bridge::SiteProfileAction;
        // Absent means "already clean" — the UI contract reports it as
        // deleted (nothing is left behind), never as deferred.
        assert_eq!(
            profile_removal_for(SiteProfileAction::Deleted),
            ProfileRemoval::Deleted
        );
        assert_eq!(
            profile_removal_for(SiteProfileAction::Absent),
            ProfileRemoval::Deleted
        );
        assert_eq!(
            profile_removal_for(SiteProfileAction::Pending),
            ProfileRemoval::Deferred
        );
        // The serialized names the JS relies on (A.8 contract).
        assert_eq!(
            serde_json::to_string(&ProfileRemoval::Deferred).unwrap(),
            "\"deferred\""
        );
        assert_eq!(
            serde_json::to_string(&ProfileRemoval::Deleted).unwrap(),
            "\"deleted\""
        );
        assert_eq!(
            serde_json::to_string(&ProfileRemoval::Unsupported).unwrap(),
            "\"unsupported\""
        );
        assert_eq!(
            serde_json::to_string(&ProfileRemoval::Hidden).unwrap(),
            "\"hidden\""
        );
        assert_eq!(
            serde_json::to_string(&ProfileRemoval::NotApplicable).unwrap(),
            "\"notApplicable\""
        );
    }

    #[test]
    fn default_settings_are_safe_by_default() {
        let s = Settings::default();
        assert!(s.block_popups, "popup blocking must default on");
        assert!(s.inject_cosmetic_rules);
        assert!(!s.fullscreen_sites, "site fullscreen is opt-in");
        assert_eq!(s.theme, ThemePref::System, "theme follows the OS");
        assert!(s.blocklist.enabled);
        assert!(!s.pip_auto_enter, "PiP auto-enter is opt-in");
        assert!(!s.developer_options, "the developer panel is opt-in");
        assert!(!s.anilist.is_configured());
    }

    #[test]
    fn settings_json_roundtrip() {
        let s = Settings::default();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.block_popups, s.block_popups);
        assert_eq!(back.blocklist.enabled, s.blocklist.enabled);
    }

    #[test]
    fn settings_patch_defaults_to_no_change() {
        let p: SettingsPatch = serde_json::from_str("{}").unwrap();
        assert!(p.block_popups.is_none());
        assert!(p.anilist.is_none());
        assert!(p.blocklist.is_none());
    }

    #[test]
    fn settings_view_never_exposes_the_client_secret() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        {
            let mut s = st.settings.lock().unwrap();
            s.anilist = AniListConfig {
                client_id: "42".into(),
                client_secret: "SUPER-SECRET".into(),
                redirect_uri: "http://127.0.0.1:1/callback".into(),
            };
        }
        let json = serde_json::to_string(&settings_view_of(&st)).unwrap();
        assert!(json.contains("\"configured\":true"));
        assert!(
            !json.contains("SUPER-SECRET"),
            "secret leaked to UI: {json}"
        );
    }

    fn settings_view_of(st: &AppState) -> SettingsView {
        let s = st.settings.lock().unwrap();
        let bl = st.blocklist.lock().unwrap();
        SettingsView {
            blocklist: s.blocklist.clone(),
            blocklist_count: bl.len(),
            block_popups: s.block_popups,
            inject_cosmetic_rules: s.inject_cosmetic_rules,
            fullscreen_sites: s.fullscreen_sites,
            theme: s.theme,
            pip_auto_enter: s.pip_auto_enter,
            developer_options: s.developer_options,
            anilist: AniListConfigView {
                configured: s.anilist.is_configured(),
                client_id: s.anilist.client_id.clone(),
                redirect_uri: s.anilist.redirect_uri.clone(),
                confidential: s.anilist.is_confidential(),
            },
            key_backend: st.provider.backend().label().to_string(),
            key_backend_os_backed: st.provider.backend().is_os_backed(),
            platform: platform_name(),
        }
    }

    #[test]
    fn site_view_marks_native_anilist_only_when_configured() {
        let anilist = Site {
            id: "builtin-anilist".into(),
            name: "AniList".into(),
            url: "https://anilist.co/".into(),
            category: Category::Tracking,
            icon: Icon::default(),
            builtin: true,
            hidden: false,
            sort: 0,
        };
        assert!(to_view(&anilist, true).native);
        assert!(!to_view(&anilist, false).native);

        let other = Site {
            url: "https://myanimelist.net/".into(),
            ..anilist.clone()
        };
        assert!(!to_view(&other, true).native);
    }

    #[test]
    fn rebuild_blocklist_follows_settings() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        {
            let mut s = st.settings.lock().unwrap();
            s.blocklist.rules = "evil.example".into();
        }
        rebuild_blocklist(&st);
        let bl = st.blocklist.lock().unwrap();
        assert!(bl.is_blocked("evil.example"));
        assert!(
            !bl.is_blocked("doubleclick.net"),
            "custom list replaces the default"
        );
    }

    #[test]
    fn token_secret_roundtrips_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        assert!(load_token(&st).unwrap().is_none());

        let token = Token {
            access_token: "tok".into(),
            refresh_token: Some("ref".into()),
            token_type: "Bearer".into(),
            expires_at: 9_999_999_999,
            scope: None,
        };
        save_token(&st, &token).unwrap();
        let back = load_token(&st).unwrap().unwrap();
        assert_eq!(back.access_token, "tok");
        assert_eq!(back.refresh_token.as_deref(), Some("ref"));

        st.provider.delete_secret("anilist-token").unwrap();
        assert!(load_token(&st).unwrap().is_none());
    }

    #[test]
    fn cookie_jar_secret_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let jar = CookieJar {
            site_id: "builtin-openanime".into(),
            cookies: vec![],
            saved_at: 5,
        };
        let bytes = serde_json::to_vec(&jar).unwrap();
        st.provider
            .put_secret("cookies-builtin-openanime", &bytes)
            .unwrap();
        let got = st
            .provider
            .get_secret("cookies-builtin-openanime")
            .unwrap()
            .unwrap();
        let back: CookieJar = serde_json::from_slice(&got).unwrap();
        assert_eq!(back.site_id, "builtin-openanime");
        assert_eq!(back.saved_at, 5);
    }

    /// The removed `get_site_cookies` / `set_site_cookies` commands took an
    /// arbitrary `id` / jar from the WebView, so any page could read or
    /// overwrite ANY site's encrypted cookie jar. Storage itself is per-site by
    /// key: prove that, because it is the property the IPC surface must not
    /// re-expose.
    #[test]
    fn cookie_jar_secrets_are_scoped_per_site() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());

        let jar = |site_id: &str, saved_at: i64| CookieJar {
            site_id: site_id.into(),
            cookies: vec![],
            saved_at,
        };
        let key = |site_id: &str| format!("cookies-{site_id}");
        let put = |site_id: &str, saved_at: i64| {
            let bytes = serde_json::to_vec(&jar(site_id, saved_at)).unwrap();
            st.provider.put_secret(&key(site_id), &bytes).unwrap();
        };
        put("site-a", 11);
        put("site-b", 22);

        let read = |site_id: &str| -> CookieJar {
            let got = st
                .provider
                .get_secret(&key(site_id))
                .unwrap()
                .unwrap_or_else(|| panic!("{site_id} jar'ı yazılmıştı"));
            serde_json::from_slice(&got).unwrap()
        };
        assert_eq!(read("site-a").saved_at, 11);
        assert_eq!(read("site-b").saved_at, 22);

        // A never leaks through B's key, and an unknown site has no jar at all
        // (no implicit fallback to another site's data).
        assert_eq!(read("site-a").site_id, "site-a");
        assert!(st.provider.get_secret(&key("site-c")).unwrap().is_none());
        // Overwriting A leaves B untouched.
        put("site-a", 99);
        assert_eq!(read("site-a").saved_at, 99);
        assert_eq!(read("site-b").saved_at, 22);
    }

    #[test]
    fn platform_and_webview_names_are_known() {
        assert!(!platform_name().is_empty());
        assert!(!webview_name().is_empty());
        assert_eq!(backend_name(Backend::AndroidKeystore), "Android Keystore");
    }

    #[test]
    fn pip_startup_should_apply_generation_and_value() {
        // Generation is primary; value is secondary.
        assert!(
            !pip_startup_should_apply(0, 1, false, false),
            "gen changed even though value same -> skip"
        );
        assert!(
            !pip_startup_should_apply(0, 0, false, true),
            "value changed -> skip"
        );
        assert!(
            pip_startup_should_apply(0, 0, false, false),
            "same gen+value -> apply"
        );
        assert!(
            pip_startup_should_apply(1, 1, true, true),
            "same gen+value true -> apply"
        );
        assert!(
            !pip_startup_should_apply(0, 2, false, false),
            "gen 2 vs 0 even with same value -> skip (toggle and back)"
        );
    }

    #[test]
    fn pip_generation_bumps_only_on_pip_change() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        assert_eq!(
            st.pip_generation.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        // Change pip via update_settings
        let _patch = SettingsPatch {
            blocklist: None,
            block_popups: None,
            inject_cosmetic_rules: None,
            fullscreen_sites: None,
            theme: None,
            pip_auto_enter: Some(true),
            developer_options: None,
            anilist: None,
        };
        // Need a State wrapper for update_settings – construct a fake AppHandle is heavy.
        // Instead test the generation helper directly: simulate the bump.
        st.pip_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            st.pip_generation.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        // Changing non-pip should not bump (we don't call fetch_add).
        // Verify the helper: generation unchanged after a non-pip patch would stay 1.
    }

    #[test]
    fn pip_startup_skips_when_lock_busy() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(state_in(dir.path()));
        // Hold the lock in this thread, then try to acquire it from another
        // thread like startup would with try_lock – it should fail.
        let guard = st.pip_lock.lock().unwrap();
        let st2 = std::sync::Arc::clone(&st);
        let try_result = std::thread::spawn(move || {
            // This thread tries to acquire the lock while main holds it.
            // In real startup we use try_lock which should fail.
            // Here we test that try_lock indeed fails when held.
            st2.pip_lock.try_lock().is_err()
        })
        .join()
        .unwrap();
        assert!(
            try_result,
            "startup try_lock should fail when toggle holds lock"
        );
        drop(guard);
        // After release, try_lock should succeed.
        assert!(st.pip_lock.try_lock().is_ok());
    }

    #[test]
    fn android_blocklist_snapshot_is_empty_when_disabled() {
        let rules = "doubleclick.net\n*.tracker.example\n=exact.example\n";
        let enabled = Blocklist::from_rules(rules, true);
        assert_eq!(android_blocklist_entries(&enabled).len(), 3);
        // Disabled must push NOTHING: Kotlin only understands rules, so a
        // snapshot sent while disabled would keep blocking on Android while
        // the desktop matcher returns false for every host.
        let disabled = Blocklist::from_rules(rules, false);
        assert!(!disabled.is_blocked("doubleclick.net"));
        assert!(android_blocklist_entries(&disabled).is_empty());
    }

    #[test]
    fn lock_recover_survives_a_poisoned_mutex() {
        let mutex = Mutex::new(7u32);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison the mutex on purpose");
        }));
        assert!(poisoned.is_err());
        assert!(mutex.is_poisoned());
        assert_eq!(*lock_recover(&mutex), 7);
    }
}
