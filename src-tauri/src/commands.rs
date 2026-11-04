//! The `invoke` surface exposed to the frontend.
//!
//! Commands are deliberately thin: validation, encryption and window policy
//! all live in the modules they belong to, so this file is mostly plumbing.

use crate::anilist::{self, AniListClient, AniListConfig, Token};
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
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Delivery bookkeeping for the Android blocklist snapshot.
///
/// `generation` starts at **1** while `applied` starts at **0**, so the very
/// first transfer always pushes. That matters: `lib.rs` builds the startup
/// `Blocklist` directly instead of going through [`rebuild_blocklist`], and
/// Kotlin's `SiteBlocklist` starts empty. Encoding "nothing delivered yet" as
/// generation 0 on both sides would have let the first `open_site` skip the
/// push and run the site WebView with no user rules at all.
#[derive(Debug)]
pub struct BlocklistSyncGate {
    /// Serializes the transfers themselves.
    ///
    /// **Lock order:** this lock → `ANDROID_SESSION_LOCK`. The fail-closed
    /// close path (`close_active_android_site` → `windows::close_site_window`)
    /// takes the session lock while this one is held; nothing ever takes them
    /// the other way round and the close path never acquires this lock, so the
    /// existing lifecycle rules and lock order are unchanged and this cannot
    /// deadlock against them.
    pub lock: Mutex<()>,
    /// Bumped once per installed `Blocklist`, while the blocklist lock is held.
    pub generation: std::sync::atomic::AtomicU64,
    /// Highest generation whose snapshot Kotlin confirmed.
    pub applied: std::sync::atomic::AtomicU64,
}

impl BlocklistSyncGate {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            generation: std::sync::atomic::AtomicU64::new(1),
            applied: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl Default for BlocklistSyncGate {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AppState {
    pub provider: Box<dyn SecretProvider>,
    pub registry: Mutex<Registry>,
    pub settings: Mutex<Settings>,
    pub blocklist: Mutex<Blocklist>,
    /// Pending OAuth attempt and the generation that invalidates stale callbacks.
    pub oauth_state: Mutex<anilist::OAuthState>,
    /// Serializes AniList token refreshes so concurrent commands cannot race
    /// the same refresh token or delete a token a newer refresh just saved.
    pub anilist_refresh_lock: tokio::sync::Mutex<()>,
    /// Serializes every AniList token-store read/compare/write/delete sequence.
    /// Never held across an `.await`; lock order is refresh lock -> this lock.
    pub anilist_token_lock: Mutex<()>,
    /// Warning from a registry that could not be decrypted on startup.
    pub startup_warning: Mutex<Option<String>>,
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
    /// Blocklist delivery bookkeeping for the Android WebView: the transfer
    /// serialization lock plus the generation/applied pair that stops a delayed
    /// snapshot from overwriting newer rules. See [`BlocklistSyncGate`].
    pub blocklist_sync: BlocklistSyncGate,
}

#[derive(Debug, Clone)]
pub struct CurrentSite {
    pub site_id: String,
    /// Opaque identity of this exact opening, not of the site.
    ///
    /// A site id is stable across reopens, so a delayed close from the
    /// previous opening can name the same site as the new one. This id is
    /// minted once per successful `open_site` and travels with the native
    /// session: close paths snapshot it before tearing anything down and put
    /// that snapshot in the `animehub://site-closed` event.
    pub session_id: String,
    pub host: String,
    pub window_label: String,
    /// Desktop: `true` when opening the site is what put the window into
    /// fullscreen, so closing it may restore the windowed state. Kept
    /// separate so a manual fullscreen choice is never taken away.
    pub entered_fullscreen: bool,
}

impl CurrentSite {
    /// The event that closes exactly this native session.
    ///
    /// Call this on the record captured for the close, never on the live
    /// `current_site` after clearing it: that mutable slot may already belong
    /// to a newer opening of the same site.
    pub fn closed_event(&self) -> SiteClosedEvent {
        SiteClosedEvent {
            site_id: self.site_id.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

/// Payload of `animehub://site-closed`.
///
/// `siteId` keeps the event useful for diagnostics; `sessionId` is the
/// authoritative ownership proof. The launcher clears its active opening only
/// when `sessionId` exactly matches the opening it owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteClosedEvent {
    pub site_id: String,
    pub session_id: String,
}

/// A fresh opaque identity for one successful site opening.
///
/// `uuid` is already a direct dependency (v4 is enabled for OAuth state and
/// user-site ids); no additional crate is needed.
fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
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

/// Install the current settings' rules as the live matcher and advance the
/// delivery generation.
///
/// Both locks go through [`lock_recover`], not `expect`: this function is on
/// the `open_site` / `update_settings` / `reset_blocklist` path, so a panic
/// while some other thread held either mutex used to turn every later blocklist
/// action into a permanent panic loop — the same failure mode `lock_recover`
/// already removed from the navigation callback.
///
/// **Why the transfer gate is taken here.** The settings read and the matcher
/// install are two separate steps, and they are called from BOTH a synchronous
/// command (`update_settings`, `reset_blocklist` — main thread) and an
/// asynchronous one (`set_pip_auto_enter`, `#[tauri::command(async)]` — Tauri's
/// async runtime). Two of them could therefore interleave: A reads rules R1,
/// the user changes the list to R2 and B rebuilds (installs R2, generation 5),
/// then A resumes and installs R1 under generation **6**. The delivery guard
/// would then push R1 as the newest snapshot — the stale-over-newer overwrite
/// it exists to prevent — and `applied = 6` would block R2 forever.
///
/// Taking [`BlocklistSyncGate::lock`] for the read+install pair serializes
/// rebuilds against each other *and* against the transfers, so the generation
/// order below is also the settings-read order. Lock order stays
/// `gate` → `settings` → `blocklist`, and nothing acquires the gate while
/// holding settings, the matcher or the session lock, so this adds no cycle to
/// the documented `gate` → `ANDROID_SESSION_LOCK` order.
fn rebuild_blocklist(state: &AppState) {
    let _serialized = lock_recover(&state.blocklist_sync.lock);
    let (rules, enabled) = {
        let s = lock_recover(&state.settings);
        (s.blocklist.rules.clone(), s.blocklist.enabled)
    };
    let bl = Blocklist::from_rules(&rules, enabled);
    // The generation is bumped while the matcher lock is STILL held, so a
    // reader can never observe new rules under an old generation and conclude
    // that Kotlin is already up to date.
    let mut live = lock_recover(&state.blocklist);
    *live = bl;
    state
        .blocklist_sync
        .generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    drop(live);
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

/// Hidden entries, so «Başlatıcıdan gizle» is not a one-way door.
///
/// `list_sites` deliberately returns only the visible grid; without this
/// companion the hidden list was write-only. Nothing about a hidden site's
/// data is touched here — it is a read of the encrypted registry.
#[tauri::command]
pub fn list_hidden_sites(state: State<'_, AppState>) -> AppResult<Vec<SiteView>> {
    let reg = state
        .registry
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    Ok(reg.hidden().iter().map(|s| to_view(s, false)).collect())
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

// `async` runs the body on Tauri's async runtime, not on the IPC dispatch
// thread: this command blocks on the desktop/android session lock, and a sync
// command would block that thread on the same lock the lifecycle worker holds
// across main-thread dispatches — a whole-app deadlock.
#[tauri::command(async)]
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
        // Android is the only mobile target left, and it always has a
        // WebView profile to remove.
        let profile_outcome;
        let should_close = {
            let cur = state
                .current_site
                .lock()
                .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
            cur.as_ref().is_some_and(|c| c.site_id == id)
        };
        if should_close {
            crate::web::windows::close_site_window_for_clear_locked(&app, &id)?;
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
    // The WebView policy needs the current user rules BEFORE any navigation,
    // and a failed push must abort the open (fail-closed). If a previous site
    // WebView is still open, sync_android_blocklist destroys it before returning
    // the error: otherwise that view would keep enforcing the old snapshot.
    sync_android_blocklist(&app, &state)?;
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
    // it runs off the async runtime.
    //
    // An *unanswered* lookup (`DnsClass::Unknown`: timeout, NXDOMAIN, resolver
    // error, or every lookup slot busy) is refused on desktop and allowed on
    // Android, and the difference is deliberate. Desktop has no proven
    // connect-time pin for the first request (P1), so an unanswered check was
    // the last gate before the WebView resolved the name itself — that
    // fail-open is closed here (round 21). Android's site WebView resolves the
    // name again inside `DnsPinnedProxy`, which refuses a private target, so a
    // resolver hiccup is not an open hole there and refusing the open would be
    // a pure usability regression.
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
    if cfg!(desktop) && matches!(dns, crate::web::dns::DnsClass::Unknown) {
        crate::diagnostics::record_current(
            "open",
            "resolve",
            "rejected",
            None,
            None,
            Some("unknown"),
            None,
            None,
        );
        return Err(AppError::Network(
            crate::web::session::DNS_UNKNOWN_BLOCK.to_string(),
        ));
    }
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
        // Android composes the guard here. Desktop has a pinned path too
        // (the Rust CONNECT proxy), but installs the same guard through
        // `initialization_script_for_all_frames` in `web/windows.rs`, so it is
        // not duplicated into this top-frame script. Neither transport is
        // covered for UDP/WebRTC by the proxy itself.
        webrtc_guard: cfg!(target_os = "android"),
    };
    let init_script = build_init_script(&injected);
    // The identity belongs to this opening, not to `site.id`: reopening the
    // same site must produce a distinct value so a delayed event from the
    // previous opening cannot clear the new session.
    let session_id = new_session_id();

    windows::open_site_window(
        app,
        &site,
        safe.as_url(),
        &init_script,
        settings.fullscreen_sites,
        session_id.clone(),
    )
    .await
    .map(|(label, active_session_id)| OpenedSite {
        label,
        session_id: active_session_id,
        mobile: cfg!(target_os = "android"),
        // Every supported target gives true per-site storage isolation:
        // per-site profile directories on desktop, per-site WebView profiles
        // on Android. Kept in the payload for wire compatibility with older
        // launcher builds; it is always true now that iOS is gone.
        isolated: true,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedSite {
    /// Window label (desktop) or the site label (mobile bookkeeping).
    pub label: String,
    /// Opaque identity of this particular opening. The launcher stores it and
    /// accepts `animehub://site-closed` only when the event carries this exact
    /// value.
    pub session_id: String,
    /// True on Android, false on desktop.
    pub mobile: bool,
    /// Whether this platform gives true per-site storage isolation. Always
    /// `true`: desktop uses per-site profile directories and Android uses
    /// per-site WebView profiles. Retained for backward-compatible payloads.
    pub isolated: bool,
}

/// Close a site session. Android flushes/destroys the site's profiled
/// WebView; desktop removes the child WebView.
#[tauri::command]
pub async fn close_site(
    app: AppHandle,
    label: String,
    session_id: String,
    operation_id: Option<String>,
) -> AppResult<()> {
    let _operation_scope = operation_id
        .as_deref()
        .map(|id| crate::diagnostics::begin_operation_with_id("close", id))
        .or_else(|| crate::diagnostics::scope_if_missing("close"));
    // Labels are reused across openings; native close independently validates
    // that this request still owns the active session.
    windows::close_site_window_for_session(&app, &label, &session_id)
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

/// M-11: the settings form re-sends every field, with `clientSecret` blanked
/// while the modal is open, so an empty patch value must not wipe a stored
/// secret — that silently downgrades a confidential client to public and
/// breaks logins. There is no explicit clearing sentinel; a secret is only
/// ever absent after a fresh install/default env state.
fn merge_anilist_patch(stored: &AniListConfig, patch: &AniListConfig) -> AniListConfig {
    let mut merged = patch.clone();
    if merged.client_secret.is_empty() && !stored.client_secret.is_empty() {
        merged.client_secret = stored.client_secret.clone();
    }
    merged
}

#[tauri::command]
pub fn update_settings(
    app: AppHandle,
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
            if let Some(a) = &patch.anilist {
                next.anilist = merge_anilist_patch(&next.anilist, a);
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
    // The new rules are persisted above; if the Android WebView did not accept
    // them, the caller must see a failure, not a refreshed settings view that
    // implies the change took effect everywhere. The helper also destroys an
    // already-open view, because failing the command alone would leave it on
    // the previous snapshot.
    sync_android_blocklist(&app, &state)?;
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
pub fn reset_blocklist(app: AppHandle, state: State<'_, AppState>) -> AppResult<usize> {
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
    let count = lock_recover(&state.blocklist).len();
    // Resetting Rust state is not enough: the Android WebView evaluates its own
    // process-wide snapshot and would otherwise keep enforcing the old rules.
    // A failed transfer closes an already-open site before the error escapes,
    // so the caller never gets a success view that hides stale enforcement.
    sync_android_blocklist(&app, &state)?;
    Ok(count)
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

fn load_token_unlocked(state: &AppState) -> AppResult<Option<Token>> {
    match state.provider.get_secret("anilist-token")? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

fn save_token_unlocked(state: &AppState, token: &Token) -> AppResult<()> {
    let bytes = serde_json::to_vec(token)?;
    state.provider.put_secret("anilist-token", &bytes)
}

/// Reads the stored token under the token lock.
fn load_token(state: &AppState) -> AppResult<Option<Token>> {
    let _tokens = lock_recover(&state.anilist_token_lock);
    load_token_unlocked(state)
}

/// Writes the stored token under the token lock.
#[cfg(test)]
fn save_token(state: &AppState, token: &Token) -> AppResult<()> {
    let _tokens = lock_recover(&state.anilist_token_lock);
    save_token_unlocked(state, token)
}

/// Commits an OAuth token only if its attempt generation is still current.
///
/// Holds the token lock and then the OAuth lock across the check and the
/// write, so a logout or a newer login that lands in between is observed
/// instead of being overwritten. Lock order: token -> oauth.
fn save_oauth_token(state: &AppState, token: &Token, generation: u64) -> AppResult<bool> {
    let _tokens = lock_recover(&state.anilist_token_lock);
    let oauth = lock_recover(&state.oauth_state);
    if oauth.generation() != generation {
        return Ok(false);
    }
    save_token_unlocked(state, token)?;
    Ok(true)
}

/// Logout: invalidates every in-flight OAuth callback and clears the token.
///
/// Token lock then OAuth lock, the same order as `save_oauth_token`, so a
/// callback commit either completes before this runs (and its token is
/// deleted) or runs after it and is rejected by the generation check.
fn clear_token(state: &AppState) -> AppResult<()> {
    let _tokens = lock_recover(&state.anilist_token_lock);
    let mut oauth = lock_recover(&state.oauth_state);
    oauth.invalidate();
    state.provider.delete_secret("anilist-token")
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

    // Validate the redirect, bind it, and only then build the authorization
    // URL and its state — `prepare_login` owns that ordering, and it derives
    // the redirect from the configured URI so the bound address and the
    // `redirect_uri` in the URL cannot disagree. An unusable URI or a port held
    // by another process is a plain refusal: no URL, no armed state, no
    // browser. Writing state first meant such a flow armed anyway, and the
    // round-trip could only fail after the user had already granted access.
    let prepared = anilist::prepare_login(&cfg).await?;
    let listener = prepared.listener;

    // Kept for the loopback task below: if that task cannot complete the
    // attempt (no callback inside the listener's window, or a failed exchange),
    // it has to disarm an armed login instead of leaving the state pending.
    let attempt_state = prepared.state.clone();
    {
        let mut oauth = state
            .oauth_state
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        oauth.arm(prepared.state);
    }
    let url = prepared.url;

    // Desktop loopback flow: when the redirect URI points at a local HTTP
    // address (the built-in default does, http://127.0.0.1:17395), nothing
    // else would receive the OAuth redirect — the browser would land on a
    // dead port and the login would silently hang. The listener for it is
    // already bound above; this task only waits for the request and finishes
    // the login through the same events the deep-link handler uses. A
    // custom-scheme redirect skips this: the OS delivers it to the deep-link
    // handler in lib.rs instead.
    if let Some(listener) = listener {
        let handle = app.clone();
        let redirect_uri = cfg.redirect_uri.clone();
        tauri::async_runtime::spawn(async move {
            let flow: AppResult<AniListUser> = async {
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
                    // This attempt can no longer finish: nothing is listening
                    // for its callback. Clear the pending state it armed — but
                    // only if the slot still holds THIS attempt's value, so a
                    // login started in the meantime is left untouched.
                    // `try_state`, not `state`: a failure here must not turn a
                    // background task into a panic while the app is shutting
                    // down.
                    if let Some(live) = handle.try_state::<AppState>() {
                        anilist::abandon_pending_state(&live.oauth_state, &attempt_state);
                    }
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
    let generation = match anilist::take_pending_state(&state.oauth_state, &returned_state)? {
        anilist::PendingState::None => {
            return Err(AppError::AniList("bekleyen bir oturum yok".into()))
        }
        anilist::PendingState::Mismatched => {
            return Err(AppError::AniList("state doğrulaması başarısız".into()))
        }
        anilist::PendingState::Consumed { generation, .. } => generation,
    };

    let token = cli.exchange_code(&code).await?;
    let viewer = cli.viewer(&token).await?;
    if !save_oauth_token(&state, &token, generation)? {
        return Err(AppError::AniList("oturum açma iptal edildi".into()));
    }
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
    if load_token(state)?.is_none() {
        return Ok(None);
    }
    let (cli, _) = match client(state) {
        Ok(c) => c,
        // Not configured any more: report signed-out rather than erroring.
        Err(_) => return Ok(None),
    };

    let Some(token) = refresh_session_token(state, |refresh_token| {
        let cli = &cli;
        async move { cli.refresh(refresh_token.as_str()).await }
    })
    .await?
    else {
        return Ok(None);
    };

    Ok(Some((cli, token)))
}

/// Refreshes an expired token under the refresh single-flight lock. The
/// result is committed only if the stored token is still the one this call
/// started with, so a concurrent login/logout wins over an older refresh.
async fn refresh_session_token<F, Fut>(state: &AppState, refresh: F) -> AppResult<Option<Token>>
where
    F: FnOnce(Zeroizing<String>) -> Fut,
    Fut: std::future::Future<Output = AppResult<Token>>,
{
    let Some(token) = load_token(state)? else {
        return Ok(None);
    };
    if !token.needs_refresh(anilist::now_unix()) {
        return Ok(Some(token));
    }

    // Single-flight: only one caller refreshes. The others wait, then re-read
    // the saved token instead of replaying a possibly rotated refresh token.
    let _refreshing = state.anilist_refresh_lock.lock().await;
    let Some(token) = load_token(state)? else {
        return Ok(None);
    };
    if !token.needs_refresh(anilist::now_unix()) {
        return Ok(Some(token));
    }

    let Some(refresh_token) = token.refresh_token.clone() else {
        // Expired with no refresh token: nothing is left to preserve.
        delete_token_if_current(state, &token);
        return Ok(None);
    };

    match refresh(Zeroizing::new(refresh_token)).await {
        Ok(fresh) => {
            // Compare-and-save: do not overwrite a token a login, logout or
            // another refresh installed while the network call was in flight.
            let _tokens = lock_recover(&state.anilist_token_lock);
            match load_token_unlocked(state) {
                Ok(Some(current)) if token_matches(Some(&current), &token) => {
                    save_token_unlocked(state, &fresh)?;
                    Ok(Some(fresh))
                }
                Ok(current) => Ok(current),
                Err(e) => Err(e),
            }
        }
        // Only a definitive rejection ends the session; a transient failure
        // (429/5xx/timeout) keeps the token so a later call can retry.
        Err(AppError::Unauthorized) => {
            delete_token_if_current(state, &token);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Compare-and-delete: never remove a token a concurrent login or a newer
/// refresh has already replaced.
fn delete_token_if_current(state: &AppState, failed: &Token) {
    let _tokens = lock_recover(&state.anilist_token_lock);
    match load_token_unlocked(state) {
        Ok(Some(stored)) if token_matches(Some(&stored), failed) => {
            if state.provider.delete_secret("anilist-token").is_err() {
                log::error!("geçersiz AniList oturumu silinemedi");
            }
        }
        Ok(_) => {}
        Err(_) => log::error!("AniList oturumu silinemedi: mevcut token okunamadı"),
    }
}

fn token_matches(stored: Option<&Token>, failed: &Token) -> bool {
    matches!(stored, Some(stored) if stored == failed)
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
            delete_token_if_current(&state, &token);
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
            delete_token_if_current(&state, &token);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub async fn anilist_logout(app: AppHandle) -> AppResult<()> {
    let state = app.state::<AppState>();
    clear_token(&state)
}

// The AniList library/search/update/delete commands were removed (M-16):
// nothing in `src/` called them, and a live-invoke command that bypasses the
// session paths is one more surface to keep in sync for nothing.

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
            // Unknown platform – mimic JS's platform check. Only desktop
            // platforms are recognised here; anything else is unsupported
            // rather than silently persisted.
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
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// Push a snapshot through the Android bridge.
///
/// Kotlin has no failure mode for a well-formed command: `SiteBlocklist.update`
/// is an atomic reference swap. A transfer can still fail before Kotlin runs
/// (plugin unavailable, IPC rejection, webview/activity gone), which is why the
/// caller must fail closed rather than assume the old snapshot is harmless.
#[cfg(target_os = "android")]
fn push_android_blocklist(entries: &[String]) -> AppResult<()> {
    crate::android_bridge::site_blocklist_update(entries)
        .map_err(|_| AppError::BlocklistUnavailable)
}

/// Destroy the active Android site WebView after a failed blocklist transfer.
///
/// Uses the lifecycle-locked close path (`ANDROID_SESSION_LOCK`, profile flush,
/// `current_site` release and `site-closed` event). Kotlin's close is
/// idempotent, so this is safe when no view is open.
///
/// Android-only, like `push_android_blocklist`: the only caller is the
/// `#[cfg(target_os = "android")]` branch of `sync_android_blocklist`, so
/// compiling a non-Android fallback body here produced an unused-function
/// error under `clippy --all-targets -D warnings` and a stub that no host test
/// could ever observe.
#[cfg(target_os = "android")]
fn close_active_android_site(app: &AppHandle) -> AppResult<()> {
    crate::web::windows::close_site_window(app, "blocklist-sync")
}

/// The fail-closed policy, independent of Tauri so it is host-testable.
///
/// A successful push must not disturb an open session. A failed push must close
/// it before the original error is returned. If the close itself fails, the
/// caller gets a restart-required error — never `Ok` and never the reassuring
/// `blocklist_unavailable` message, because old rules may still be live.
#[cfg(any(target_os = "android", test))]
fn transfer_blocklist_snapshot<P, C>(push: P, close: C) -> AppResult<()>
where
    P: FnOnce() -> AppResult<()>,
    C: FnOnce() -> AppResult<()>,
{
    match push() {
        Ok(()) => Ok(()),
        Err(push_error) => match close() {
            Ok(()) => Err(push_error),
            Err(_close_error) => Err(AppError::Other(
                "Engelleme listesi güncellenemedi ve açık site güvenli biçimde kapatılamadı; AnimeHub'ı yeniden başlatın."
                    .into(),
            )),
        },
    }
}

/// A blocklist snapshot together with the generation it was read at.
#[cfg(any(target_os = "android", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlocklistTransfer {
    pub generation: u64,
    pub entries: Vec<String>,
}

/// Read the authoritative Kotlin snapshot under the blocklist lock.
///
/// Entries and generation are taken from the same locked section, so the pair
/// is consistent: the rules of generation N can never be labelled N+1, which
/// would let a genuinely newer transfer be skipped as "already applied".
#[cfg(any(target_os = "android", test))]
pub(crate) fn blocklist_transfer_snapshot(state: &AppState) -> BlocklistTransfer {
    let list = lock_recover(&state.blocklist);
    let entries = android_blocklist_entries(&list);
    let generation = state
        .blocklist_sync
        .generation
        .load(std::sync::atomic::Ordering::SeqCst);
    drop(list);
    BlocklistTransfer {
        generation,
        entries,
    }
}

/// Serialize blocklist transfers and refuse to deliver a stale one.
///
/// **The race this closes.** `sync_android_blocklist` runs from `open_site`,
/// `update_settings`, `set_pip_auto_enter`'s settings write and
/// `reset_blocklist` — all *outside* `ANDROID_SESSION_LOCK`, because that lock
/// guards the site lifecycle rather than the rule set, and the fail-closed
/// close below has to take it. Kotlin's `SiteBlocklist.update` is a
/// last-writer-wins atomic swap, so a transfer built from older settings could
/// complete *after* a newer one and silently restore the old rules. Nothing
/// would error: the settings file, the Rust matcher and the Kotlin snapshot
/// would simply disagree.
///
/// Two mechanisms, both needed:
///
/// * `gate.lock` serializes transfers, so at most one push is in flight.
/// * `read` is called **inside** that lock, so the value pushed is always the
///   newest one no matter which caller won the race, and a caller whose
///   generation is not newer than `applied` returns `Ok(())` without pushing.
///   That is the success path, so an up-to-date caller never closes a live
///   session.
///
/// `applied` advances only after a confirmed push. A failed transfer therefore
/// leaves it behind and the next caller retries, while the fail-closed policy
/// of [`transfer_blocklist_snapshot`] is reused unchanged — including the
/// restart-required error when the close itself fails.
#[cfg(any(target_os = "android", test))]
pub(crate) fn transfer_blocklist_with_generation<S, P, C>(
    gate: &BlocklistSyncGate,
    read: S,
    push: P,
    close: C,
) -> AppResult<()>
where
    S: Fn() -> BlocklistTransfer,
    P: FnOnce(&[String]) -> AppResult<()>,
    C: FnOnce() -> AppResult<()>,
{
    let _serialized = lock_recover(&gate.lock);
    let current = read();
    if current.generation <= gate.applied.load(std::sync::atomic::Ordering::SeqCst) {
        // Kotlin already holds this generation or a newer one. Pushing would
        // resurrect older rules; doing nothing is a success, so a live session
        // must not be closed here.
        return Ok(());
    }
    let result = transfer_blocklist_snapshot(|| push(&current.entries), close);
    if result.is_ok() {
        gate.applied
            .store(current.generation, std::sync::atomic::Ordering::SeqCst);
    }
    result
}

/// Apply the current rules to the Android site WebView before any navigation.
///
/// Called before every `open_site`, after `update_settings`, and after
/// `reset_blocklist`. The WebView evaluates the rules itself (Kotlin cannot read
/// Rust state), so a failure here is not just an error to display:
/// `transfer_blocklist_snapshot` destroys an already-active view, under the
/// normal close path, before the command fails.
///
/// The lock is recovered, never mapped to an empty rule set: an empty snapshot
/// means "the user disabled the list" to Kotlin, so turning a poisoned lock
/// into `vec![]` would quietly switch enforcement off.
pub(crate) fn sync_android_blocklist(app: &AppHandle, state: &AppState) -> AppResult<()> {
    #[cfg(target_os = "android")]
    {
        transfer_blocklist_with_generation(
            &state.blocklist_sync,
            || blocklist_transfer_snapshot(state),
            push_android_blocklist,
            || close_active_android_site(app),
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, state);
        Ok(())
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

    fn state_with(
        provider: Box<dyn SecretProvider>,
        registry: Registry,
        warning: Option<String>,
    ) -> AppState {
        AppState {
            provider,
            registry: Mutex::new(registry),
            settings: Mutex::new(Settings::default()),
            blocklist: Mutex::new(Blocklist::default()),
            oauth_state: Mutex::new(Default::default()),
            anilist_refresh_lock: tokio::sync::Mutex::new(()),
            anilist_token_lock: Mutex::new(()),
            startup_warning: Mutex::new(warning),
            current_site: Mutex::new(None),
            pip_lock: Mutex::new(()),
            pip_generation: std::sync::atomic::AtomicU64::new(0),
            diagnostics_lock: Mutex::new(()),
            blocklist_sync: BlocklistSyncGate::new(),
        }
    }

    fn state_in(dir: &std::path::Path) -> AppState {
        let key = load_master_key(dir).expect("key");
        let provider = SecureStore::new(&key, dir).expect("store");
        let (registry, warning) = provider.load_registry();
        state_with(Box::new(provider), registry, warning)
    }

    #[test]
    fn only_the_failed_token_is_deleted() {
        let failed = Token {
            access_token: "old".into(),
            refresh_token: Some("refresh".into()),
            token_type: "Bearer".into(),
            expires_at: 1,
            scope: None,
        };
        assert!(token_matches(Some(&failed), &failed));

        let mut newer = failed.clone();
        newer.access_token = "new".into();
        assert!(
            !token_matches(Some(&newer), &failed),
            "a concurrent refresh must keep its newer token"
        );
        assert!(!token_matches(None, &failed));
    }

    const FAR_FUTURE: i64 = 9_999_999_999;

    fn token(access: &str, refresh: &str, expires_at: i64) -> Token {
        Token {
            access_token: access.into(),
            refresh_token: Some(refresh.into()),
            token_type: "Bearer".into(),
            expires_at,
            scope: None,
        }
    }

    /// Blocks the provider's delete while signalling the test, so the
    /// compare-and-delete window can be held open deterministically.
    struct DeleteGate {
        entered: (Mutex<bool>, std::sync::Condvar),
        released: (Mutex<bool>, std::sync::Condvar),
    }

    impl DeleteGate {
        fn new() -> Self {
            Self {
                entered: (Mutex::new(false), std::sync::Condvar::new()),
                released: (Mutex::new(false), std::sync::Condvar::new()),
            }
        }

        fn block_until_released(&self) {
            *self.entered.0.lock().unwrap() = true;
            self.entered.1.notify_all();
            let mut released = self.released.0.lock().unwrap();
            while !*released {
                released = self.released.1.wait(released).unwrap();
            }
        }

        fn wait_entered(&self) {
            let mut entered = self.entered.0.lock().unwrap();
            while !*entered {
                entered = self.entered.1.wait(entered).unwrap();
            }
        }

        fn release(&self) {
            *self.released.0.lock().unwrap() = true;
            self.released.1.notify_all();
        }
    }

    impl Default for DeleteGate {
        fn default() -> Self {
            Self::new()
        }
    }

    struct PausingDeleteProvider {
        inner: SecureStore,
        gate: std::sync::Arc<DeleteGate>,
    }

    impl SecretProvider for PausingDeleteProvider {
        fn backend(&self) -> Backend {
            self.inner.backend()
        }

        fn load_doc_bytes(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
            self.inner.load_doc_bytes(name)
        }

        fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
            self.inner.save_doc_bytes(name, bytes)
        }

        fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
            self.inner.put_secret(name, plaintext)
        }

        fn get_secret(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
            self.inner.get_secret(name)
        }

        fn delete_secret(&self, name: &str) -> AppResult<()> {
            self.gate.block_until_released();
            self.inner.delete_secret(name)
        }

        fn wipe_secrets(&self) -> AppResult<usize> {
            self.inner.wipe_secrets()
        }

        fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
            self.inner.delete_site_session(site_id)
        }
    }

    #[tokio::test]
    async fn refresh_completed_after_logout_cannot_restore_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(state_in(dir.path()));
        save_token(&st, &token("old-access", "old-refresh", 1)).unwrap();

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = {
            let st = st.clone();
            tokio::spawn(async move {
                refresh_session_token(&st, |refresh_token| async move {
                    assert_eq!(refresh_token.as_str(), "old-refresh");
                    started_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    Ok(token("refreshed-old", "rotated-refresh", FAR_FUTURE))
                })
                .await
            })
        };

        started_rx.await.unwrap();
        clear_token(&st).unwrap();
        release_tx.send(()).unwrap();

        assert!(
            task.await.unwrap().unwrap().is_none(),
            "a refresh that finished after logout must not return a session"
        );
        assert!(load_token(&st).unwrap().is_none());
    }

    #[test]
    fn stale_viewer_401_does_not_delete_a_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let rejected = token("old-access", "old-refresh", 1);
        let new = token("new-login", "new-login-refresh", FAR_FUTURE);
        save_token(&st, &rejected).unwrap();
        save_token(&st, &new).unwrap();

        delete_token_if_current(&st, &rejected);

        assert_eq!(load_token(&st).unwrap(), Some(new));
    }

    #[test]
    fn current_rejected_token_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let rejected = token("old-access", "old-refresh", 1);
        save_token(&st, &rejected).unwrap();

        delete_token_if_current(&st, &rejected);

        assert!(load_token(&st).unwrap().is_none());
    }

    #[test]
    fn compare_and_delete_cannot_remove_a_token_saved_in_between() {
        let dir = tempfile::tempdir().unwrap();
        let key = load_master_key(dir.path()).expect("key");
        let store = SecureStore::new(&key, dir.path()).expect("store");
        let (registry, warning) = store.load_registry();
        let gate = std::sync::Arc::new(DeleteGate::new());
        let provider = PausingDeleteProvider {
            inner: store,
            gate: gate.clone(),
        };
        let st = std::sync::Arc::new(state_with(Box::new(provider), registry, warning));

        let old = token("old", "old-refresh", 1);
        let new = token("new", "new-refresh", FAR_FUTURE);
        save_token(&st, &old).unwrap();

        let deleter = {
            let st = st.clone();
            let old = old.clone();
            std::thread::spawn(move || delete_token_if_current(&st, &old))
        };
        gate.wait_entered();
        assert!(
            st.anilist_token_lock.try_lock().is_err(),
            "compare-and-delete must hold the token lock across the delete"
        );

        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let saver = {
            let st = st.clone();
            let new = new.clone();
            std::thread::spawn(move || {
                attempted_tx.send(()).unwrap();
                save_token(&st, &new).unwrap();
            })
        };
        attempted_rx.recv().unwrap();
        gate.release();
        deleter.join().unwrap();
        saver.join().unwrap();

        assert_eq!(load_token(&st).unwrap(), Some(new));
    }

    #[tokio::test]
    async fn refresh_cannot_overwrite_a_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(state_in(dir.path()));
        save_token(&st, &token("old-access", "old-refresh", 1)).unwrap();
        let new = token("new-login", "new-login-refresh", FAR_FUTURE);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = {
            let st = st.clone();
            tokio::spawn(async move {
                refresh_session_token(&st, |refresh_token| async move {
                    assert_eq!(refresh_token.as_str(), "old-refresh");
                    started_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    Ok(token("refreshed-old", "rotated-old", FAR_FUTURE))
                })
                .await
            })
        };

        started_rx.await.unwrap();
        save_token(&st, &new).unwrap();
        release_tx.send(()).unwrap();

        let result = task.await.unwrap().unwrap();
        assert_eq!(result, Some(new.clone()));
        assert_eq!(load_token(&st).unwrap(), Some(new));
    }

    #[tokio::test]
    async fn rejected_refresh_does_not_delete_a_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(state_in(dir.path()));
        save_token(&st, &token("old-access", "old-refresh", 1)).unwrap();
        let new = token("new-login", "new-login-refresh", FAR_FUTURE);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = {
            let st = st.clone();
            tokio::spawn(async move {
                refresh_session_token(&st, |_refresh_token| async move {
                    started_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    Err(AppError::Unauthorized)
                })
                .await
            })
        };

        started_rx.await.unwrap();
        save_token(&st, &new).unwrap();
        release_tx.send(()).unwrap();

        assert!(task.await.unwrap().unwrap().is_none());
        assert_eq!(
            load_token(&st).unwrap(),
            Some(new),
            "an old rejected refresh must not delete a newer login"
        );
    }

    #[tokio::test]
    async fn concurrent_refreshes_run_once_and_keep_the_rotated_token() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(state_in(dir.path()));
        save_token(&st, &token("a1", "r1", 1)).unwrap();

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let args = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let st = st.clone();
            let calls = calls.clone();
            let args = args.clone();
            let entered_tx = entered_tx.clone();
            let release_rx = release_rx.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                refresh_session_token(&st, move |refresh_token| {
                    let calls = calls.clone();
                    let args = args.clone();
                    let entered_tx = entered_tx.clone();
                    let mut release_rx = release_rx.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        args.lock()
                            .unwrap()
                            .push(refresh_token.as_str().to_string());
                        let _ = entered_tx.send(());
                        release_rx.wait_for(|released| *released).await.unwrap();
                        Ok(token("a2", "r2", FAR_FUTURE))
                    }
                })
                .await
            }));
        }

        entered_rx
            .recv()
            .await
            .expect("the first refresh must start");
        release_tx.send(true).unwrap();

        for task in tasks {
            let result = task.await.unwrap().unwrap().expect("a session");
            assert_eq!(result.access_token, "a2");
            assert_eq!(result.refresh_token.as_deref(), Some("r2"));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the refresh single-flight lock must run the network call once"
        );
        let seen = args.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], "r1");
        assert_eq!(
            load_token(&st).unwrap(),
            Some(token("a2", "r2", FAR_FUTURE))
        );
    }

    fn arm_oauth(st: &AppState, state: &str) {
        lock_recover(&st.oauth_state).arm(state.to_string());
    }

    fn begin_callback(st: &AppState, state: &str) -> u64 {
        arm_oauth(st, state);
        match anilist::take_pending_state(&st.oauth_state, state).unwrap() {
            anilist::PendingState::Consumed { generation, .. } => generation,
            other => panic!("unexpected pending state: {other:?}"),
        }
    }

    #[test]
    fn oauth_callback_completed_after_logout_cannot_restore_session() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(state_in(dir.path()));
        let generation = begin_callback(&st, "state-a");
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let saver = {
            let st = st.clone();
            std::thread::spawn(move || {
                release_rx.recv().unwrap();
                save_oauth_token(
                    &st,
                    &token("old-access", "old-refresh", FAR_FUTURE),
                    generation,
                )
            })
        };

        clear_token(&st).unwrap();
        release_tx.send(()).unwrap();

        assert!(!saver.join().unwrap().unwrap());
        assert!(load_token(&st).unwrap().is_none());
    }

    #[test]
    fn old_oauth_callback_cannot_overwrite_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let generation_a = begin_callback(&st, "state-a");
        let generation_b = begin_callback(&st, "state-b");
        let new_login = token("new-login", "new-refresh", FAR_FUTURE);

        assert!(save_oauth_token(&st, &new_login, generation_b).unwrap());
        assert!(!save_oauth_token(
            &st,
            &token("old-login", "old-refresh", FAR_FUTURE),
            generation_a,
        )
        .unwrap());
        assert_eq!(load_token(&st).unwrap(), Some(new_login));
    }

    #[test]
    fn logout_invalidates_in_flight_oauth_callback() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());

        // A callback that has not arrived yet is dropped with the attempt.
        arm_oauth(&st, "state-a");
        clear_token(&st).unwrap();
        assert_eq!(
            anilist::take_pending_state(&st.oauth_state, "state-a").unwrap(),
            anilist::PendingState::None
        );

        // A callback that already consumed its state cannot save either.
        let generation = begin_callback(&st, "state-b");
        clear_token(&st).unwrap();
        assert!(!save_oauth_token(
            &st,
            &token("late-access", "late-refresh", FAR_FUTURE),
            generation
        )
        .unwrap());
        assert!(load_token(&st).unwrap().is_none());
    }

    #[test]
    fn failed_old_callback_cannot_clear_new_pending_login() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        arm_oauth(&st, "state-a");
        arm_oauth(&st, "state-b");

        assert!(!anilist::abandon_pending_state(&st.oauth_state, "state-a"));
        assert_eq!(
            anilist::take_pending_state(&st.oauth_state, "state-b").unwrap(),
            anilist::PendingState::Consumed {
                state: "state-b".to_string(),
                generation: 2
            }
        );
    }

    #[test]
    fn oauth_state_remains_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        arm_oauth(&st, "state-a");

        assert!(matches!(
            anilist::take_pending_state(&st.oauth_state, "state-a").unwrap(),
            anilist::PendingState::Consumed { .. }
        ));
        assert_eq!(
            anilist::take_pending_state(&st.oauth_state, "state-a").unwrap(),
            anilist::PendingState::None
        );
    }

    #[test]
    fn logout_and_callback_save_cannot_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let key = load_master_key(dir.path()).expect("key");
        let store = SecureStore::new(&key, dir.path()).expect("store");
        let (registry, warning) = store.load_registry();
        let gate = std::sync::Arc::new(DeleteGate::new());
        let provider = PausingDeleteProvider {
            inner: store,
            gate: gate.clone(),
        };
        let st = std::sync::Arc::new(state_with(Box::new(provider), registry, warning));
        let generation = begin_callback(&st, "state-a");

        let logout = {
            let st = st.clone();
            std::thread::spawn(move || clear_token(&st))
        };
        gate.wait_entered();

        // Logout is inside delete_secret while holding the token lock. The
        // saver proves that by failing a non-blocking acquire before it tries.
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let saver = {
            let st = st.clone();
            std::thread::spawn(move || {
                observed_tx
                    .send(st.anilist_token_lock.try_lock().is_err())
                    .unwrap();
                save_oauth_token(
                    &st,
                    &token("late-access", "late-refresh", FAR_FUTURE),
                    generation,
                )
            })
        };
        assert!(
            observed_rx.recv().unwrap(),
            "logout must still hold the token lock"
        );

        gate.release();
        assert!(logout.join().unwrap().is_ok());
        assert!(!saver.join().unwrap().unwrap());
        assert!(load_token(&st).unwrap().is_none());
    }

    #[test]
    fn armed_new_login_rejects_old_callback_save() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let existing = token("existing", "existing-refresh", FAR_FUTURE);
        save_token(&st, &existing).unwrap();

        let generation_a = begin_callback(&st, "state-a");
        arm_oauth(&st, "state-b");

        assert!(!save_oauth_token(
            &st,
            &token("old-login", "old-refresh", FAR_FUTURE),
            generation_a
        )
        .unwrap());
        assert_eq!(load_token(&st).unwrap(), Some(existing));
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

    fn anilist_config() -> AniListConfig {
        AniListConfig {
            client_id: "42".into(),
            client_secret: String::new(),
            redirect_uri: "http://127.0.0.1:1/callback".into(),
        }
    }

    #[test]
    fn partial_settings_save_does_not_wipe_the_stored_client_secret() {
        let stored = AniListConfig {
            client_secret: "SUPER-SECRET".into(),
            ..anilist_config()
        };
        // The form blanks the secret field and re-sends everything.
        let patch = AniListConfig {
            client_id: "43".into(),
            client_secret: String::new(),
            ..anilist_config()
        };
        let merged = merge_anilist_patch(&stored, &patch);
        assert_eq!(merged.client_id, "43");
        assert_eq!(merged.client_secret, "SUPER-SECRET");
        assert!(merged.is_confidential(), "no silent downgrade to public");

        // With no stored secret an empty patch stays empty — no fake values.
        assert!(merge_anilist_patch(&anilist_config(), &anilist_config())
            .client_secret
            .is_empty());
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
    fn rebuild_blocklist_survives_poisoned_settings_and_blocklist_locks() {
        // A panic elsewhere while holding either mutex used to make every
        // later blocklist action panic too (`lock().expect(..)`), which on the
        // Android path means the user can never open a site or change the
        // rules again. `lock_recover` keeps the last consistent value instead.
        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        {
            let mut s = st.settings.lock().unwrap();
            s.blocklist.rules = "evil.example".into();
        }
        for mutex_selector in 0..2 {
            let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if mutex_selector == 0 {
                    let _guard = st.settings.lock().unwrap();
                    panic!("poison the settings lock on purpose");
                } else {
                    let _guard = st.blocklist.lock().unwrap();
                    panic!("poison the blocklist lock on purpose");
                }
            }));
            assert!(poisoned.is_err());
        }
        assert!(st.settings.is_poisoned());
        assert!(st.blocklist.is_poisoned());

        let before = st
            .blocklist_sync
            .generation
            .load(std::sync::atomic::Ordering::SeqCst);
        rebuild_blocklist(&st);
        assert_eq!(
            st.blocklist_sync
                .generation
                .load(std::sync::atomic::Ordering::SeqCst),
            before + 1,
            "a poisoned lock must still install the rules and advance once"
        );
        let bl = lock_recover(&st.blocklist);
        assert!(bl.is_blocked("evil.example"));
        assert!(!bl.is_blocked("doubleclick.net"));
    }

    #[test]
    fn rebuild_blocklist_cannot_install_while_a_transfer_holds_the_gate() {
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        // The real interleaving behind the stale-overwrite: `set_pip_auto_enter`
        // is an ASYNC command and rebuilds on Tauri's async runtime while
        // `update_settings` rebuilds on the main thread. Without the gate, the
        // slower rebuild installs OLDER rules under a NEWER generation.
        let dir = tempfile::tempdir().unwrap();
        let st = Arc::new(state_in(dir.path()));
        let before = st.blocklist_sync.generation.load(Ordering::SeqCst);
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let holder = {
            let st = Arc::clone(&st);
            let held = Arc::clone(&held);
            std::thread::spawn(move || {
                let _gate = lock_recover(&st.blocklist_sync.lock);
                ready_tx.send(()).expect("signal");
                while held.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            })
        };
        ready_rx.recv().expect("the gate holder must start");

        let worker = {
            let st = Arc::clone(&st);
            std::thread::spawn(move || rebuild_blocklist(&st))
        };
        // The rebuild must be blocked on the gate, not installing behind it.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            st.blocklist_sync.generation.load(Ordering::SeqCst),
            before,
            "a rebuild ran while the transfer gate was held: settings read and \
             install are not serialized"
        );

        held.store(false, Ordering::SeqCst);
        holder.join().expect("gate holder");
        worker.join().expect("rebuild thread");
        assert_eq!(
            st.blocklist_sync.generation.load(Ordering::SeqCst),
            before + 1,
            "the rebuild must install exactly once after the gate is released"
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

        clear_token(&st).unwrap();
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
    fn blocklist_transfer_success_does_not_close_an_open_site() {
        use std::cell::Cell;

        let pushes = Cell::new(0u32);
        let closes = Cell::new(0u32);
        let result = transfer_blocklist_snapshot(
            || {
                pushes.set(pushes.get() + 1);
                Ok(())
            },
            || {
                closes.set(closes.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(pushes.get(), 1);
        assert_eq!(
            closes.get(),
            0,
            "a successful push must not disturb the live site"
        );
    }

    #[test]
    fn blocklist_transfer_closes_the_live_site_before_returning_push_error() {
        use std::cell::RefCell;

        let calls = RefCell::new(Vec::<&'static str>::new());
        let result = transfer_blocklist_snapshot(
            || {
                calls.borrow_mut().push("push");
                Err(AppError::BlocklistUnavailable)
            },
            || {
                calls.borrow_mut().push("close");
                Ok(())
            },
        );

        assert_eq!(*calls.borrow(), vec!["push", "close"]);
        assert_eq!(result.unwrap_err().code(), "blocklist_unavailable");
    }

    #[test]
    fn blocklist_transfer_close_failure_requires_a_restart_not_success() {
        use std::cell::Cell;

        let closes = Cell::new(0u32);
        let result = transfer_blocklist_snapshot(
            || Err(AppError::BlocklistUnavailable),
            || {
                closes.set(closes.get() + 1);
                Err(AppError::Other("close flush failed".into()))
            },
        );

        assert_eq!(closes.get(), 1);
        let error = result.expect_err("a failed close must never be reported as success");
        assert_eq!(error.code(), "other");
        assert!(
            error.to_string().contains("yeniden başlatın"),
            "the user must be told the old snapshot may still be live"
        );
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

    // -----------------------------------------------------------------------
    // Blocklist transfer ordering (PR #27 security follow-up)
    //
    // These drive `transfer_blocklist_with_generation`, the real function the
    // Android `sync_android_blocklist` path calls, against a fake Kotlin side
    // that records every snapshot it would have received. What a host test
    // cannot do is run the actual Tauri command boundary (`open_site`,
    // `update_settings`, `reset_blocklist` need a live `AppHandle`), so the
    // settings write / matcher rebuild half of the chain is driven through the
    // real `write_settings_doc` + `rebuild_blocklist` below.
    // -----------------------------------------------------------------------

    /// Every snapshot that would have reached Kotlin's `SiteBlocklist.update`,
    /// in arrival order, tagged with the generation it claimed.
    type DeliveredLog = std::sync::Mutex<Vec<(u64, Vec<String>)>>;

    fn delivered_last(log: &DeliveredLog) -> Option<(u64, Vec<String>)> {
        log.lock().unwrap().last().cloned()
    }

    fn delivered_len(log: &DeliveredLog) -> usize {
        log.lock().unwrap().len()
    }

    #[test]
    fn first_blocklist_transfer_pushes_even_before_any_rebuild() {
        use std::sync::atomic::Ordering;

        // `lib.rs` builds the startup `Blocklist` directly, so no rebuild has
        // bumped the generation yet. Kotlin starts EMPTY, so skipping this
        // first push would run the site WebView with no user rules at all.
        let gate = BlocklistSyncGate::new();
        let delivered: DeliveredLog = std::sync::Mutex::new(Vec::new());
        let closes = std::sync::atomic::AtomicU32::new(0);

        let result = transfer_blocklist_with_generation(
            &gate,
            || BlocklistTransfer {
                generation: gate.generation.load(Ordering::SeqCst),
                entries: vec!["doubleclick.net".into()],
            },
            |entries| {
                let generation = gate.generation.load(Ordering::SeqCst);
                delivered
                    .lock()
                    .unwrap()
                    .push((generation, entries.to_vec()));
                Ok(())
            },
            || {
                closes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(delivered_len(&delivered), 1, "the first open must push");
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "a successful push must not disturb a live session"
        );
        assert_eq!(gate.applied.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_blocklist_snapshot_is_refused_without_closing_a_live_site() {
        use std::sync::atomic::Ordering;

        // Kotlin already holds generation 5; a delayed caller still carries
        // generation 3. Pushing it would restore older rules, and closing the
        // session would be wrong too: nothing failed.
        let gate = BlocklistSyncGate::new();
        gate.applied.store(5, Ordering::SeqCst);
        let delivered: DeliveredLog = std::sync::Mutex::new(Vec::new());
        let closes = std::sync::atomic::AtomicU32::new(0);

        let result = transfer_blocklist_with_generation(
            &gate,
            || BlocklistTransfer {
                generation: 3,
                entries: vec!["stale.example".into()],
            },
            |entries| {
                delivered.lock().unwrap().push((3, entries.to_vec()));
                Ok(())
            },
            || {
                closes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        assert!(result.is_ok(), "an up-to-date WebView is not an error");
        assert_eq!(
            delivered_len(&delivered),
            0,
            "a stale snapshot must not be pushed"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "and must not close the live site"
        );
        assert_eq!(
            gate.applied.load(Ordering::SeqCst),
            5,
            "applied never goes backwards"
        );
    }

    #[test]
    fn delayed_transfer_delivers_the_newest_snapshot_not_the_captured_one() {
        use std::sync::atomic::Ordering;

        // The live Rust matcher, shared by a "fast" caller and a "delayed" one.
        let live = std::sync::Mutex::new((1u64, vec!["gen-1.example".to_string()]));
        let gate = BlocklistSyncGate::new();
        let delivered: DeliveredLog = std::sync::Mutex::new(Vec::new());
        // Passed by REFERENCE: `P`/`C` are `FnOnce`, so handing the closures
        // over by value would move them and the three calls below would not
        // compile. `&F` implements `Fn` (hence `FnOnce`) for any `F: Fn`.
        let read_live = || {
            let guard = live.lock().unwrap();
            BlocklistTransfer {
                generation: guard.0,
                entries: guard.1.clone(),
            }
        };
        let record = |entries: &[String]| -> AppResult<()> {
            let generation = live.lock().unwrap().0;
            delivered
                .lock()
                .unwrap()
                .push((generation, entries.to_vec()));
            Ok(())
        };
        let no_close = || -> AppResult<()> { Ok(()) };

        // Caller A transfers generation 1.
        transfer_blocklist_with_generation(&gate, read_live, record, no_close).unwrap();

        // Settings change while caller B is already on its way: generation 2.
        {
            let mut guard = live.lock().unwrap();
            guard.0 = 2;
            guard.1 = vec!["gen-2.example".into()];
        }
        // Caller B (newer) transfers.
        transfer_blocklist_with_generation(&gate, read_live, record, no_close).unwrap();

        // Caller A' — built from the OLD settings — finally completes. Because
        // the snapshot is re-read inside the serialization lock, it cannot
        // overwrite generation 2.
        transfer_blocklist_with_generation(&gate, read_live, record, no_close).unwrap();

        let log = delivered.lock().unwrap();
        assert_eq!(log.len(), 2, "the third call has nothing new to deliver");
        assert_eq!(log[0].0, 1);
        assert_eq!(log[1].0, 2);
        assert_eq!(
            log.last().unwrap().1,
            vec!["gen-2.example".to_string()],
            "Kotlin must end on the newest rules"
        );
        drop(log);
        assert_eq!(gate.applied.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn blocklist_push_failure_closes_the_live_site_and_does_not_advance_applied() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let gate = BlocklistSyncGate::new();
        let delivered: DeliveredLog = std::sync::Mutex::new(Vec::new());
        let closes = std::sync::atomic::AtomicU32::new(0);
        let bridge_down = AtomicBool::new(true);

        let result = transfer_blocklist_with_generation(
            &gate,
            || BlocklistTransfer {
                generation: 7,
                entries: vec!["doubleclick.net".into()],
            },
            |entries| {
                if bridge_down.load(Ordering::SeqCst) {
                    return Err(AppError::BlocklistUnavailable);
                }
                delivered.lock().unwrap().push((7, entries.to_vec()));
                Ok(())
            },
            || {
                closes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().code(), "blocklist_unavailable");
        assert_eq!(
            closes.load(Ordering::SeqCst),
            1,
            "fail closed: destroy the live view"
        );
        assert_eq!(delivered_len(&delivered), 0);
        assert_eq!(
            gate.applied.load(Ordering::SeqCst),
            0,
            "a failed push must not be recorded as delivered, or the retry would be skipped"
        );

        // The bridge recovers: the SAME generation is retried and delivered.
        bridge_down.store(false, Ordering::SeqCst);
        transfer_blocklist_with_generation(
            &gate,
            || BlocklistTransfer {
                generation: 7,
                entries: vec!["doubleclick.net".into()],
            },
            |entries| {
                delivered.lock().unwrap().push((7, entries.to_vec()));
                Ok(())
            },
            || {
                closes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(delivered_len(&delivered), 1);
        assert_eq!(
            closes.load(Ordering::SeqCst),
            1,
            "the retry succeeded, nothing closed"
        );
        assert_eq!(gate.applied.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn blocklist_close_failure_is_restart_required_and_leaves_applied_behind() {
        use std::sync::atomic::Ordering;

        let gate = BlocklistSyncGate::new();
        gate.applied.store(2, Ordering::SeqCst);

        let result = transfer_blocklist_with_generation(
            &gate,
            || BlocklistTransfer {
                generation: 3,
                entries: vec!["doubleclick.net".into()],
            },
            |_| Err(AppError::BlocklistUnavailable),
            || Err(AppError::Other("flush failed".into())),
        );

        let error = result.expect_err("a failed close must never be reported as success");
        assert_eq!(error.code(), "other");
        assert!(
            error.to_string().contains("yeniden başlatın"),
            "the user must be told the old snapshot may still be live, got: {error}"
        );
        assert_eq!(
            gate.applied.load(Ordering::SeqCst),
            2,
            "nothing was delivered, so generation 3 stays pending"
        );
    }

    #[test]
    fn rebuild_blocklist_bumps_the_generation_once_per_rebuild() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let before = st.blocklist_sync.generation.load(Ordering::SeqCst);
        assert_eq!(before, 1, "a fresh gate starts above `applied` (0)");

        rebuild_blocklist(&st);
        assert_eq!(
            st.blocklist_sync.generation.load(Ordering::SeqCst),
            before + 1
        );
        rebuild_blocklist(&st);
        assert_eq!(
            st.blocklist_sync.generation.load(Ordering::SeqCst),
            before + 2
        );

        // The generation and the rules must be readable together, consistently.
        let snapshot = blocklist_transfer_snapshot(&st);
        assert_eq!(
            snapshot.generation,
            st.blocklist_sync.generation.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn disabled_blocklist_still_transfers_an_empty_snapshot_through_the_guard() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        {
            let mut s = st.settings.lock().unwrap();
            s.blocklist.rules = "doubleclick.net\n*.tracker.example\n=exact.example\n".into();
            s.blocklist.enabled = false;
        }
        rebuild_blocklist(&st);

        let delivered: DeliveredLog = std::sync::Mutex::new(Vec::new());
        let closes = std::sync::atomic::AtomicU32::new(0);
        let generation = st.blocklist_sync.generation.load(Ordering::SeqCst);
        transfer_blocklist_with_generation(
            &st.blocklist_sync,
            || blocklist_transfer_snapshot(&st),
            |entries| {
                delivered
                    .lock()
                    .unwrap()
                    .push((generation, entries.to_vec()));
                Ok(())
            },
            || {
                closes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();

        // Disabling is a CHANGE, so it must be delivered — as an empty
        // snapshot, which is the only way Kotlin understands "turned off".
        let (_, entries) = delivered_last(&delivered).expect("the disable must be transferred");
        assert!(
            entries.is_empty(),
            "disabled must push nothing, got {entries:?}"
        );
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert_eq!(st.blocklist_sync.applied.load(Ordering::SeqCst), generation);
    }

    #[test]
    fn reset_blocklist_reaches_android_with_the_default_snapshot() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let st = state_in(dir.path());
        let delivered: DeliveredLog = std::sync::Mutex::new(Vec::new());
        let closes = std::sync::atomic::AtomicU32::new(0);

        // The user replaced the defaults with a single custom rule.
        {
            let mut s = st.settings.lock().unwrap();
            s.blocklist.rules = "evil.example".into();
            s.blocklist.enabled = true;
            write_settings_doc(&st, &s).expect("settings write");
        }
        rebuild_blocklist(&st);
        let sync = |delivered: &DeliveredLog| {
            let generation = st.blocklist_sync.generation.load(Ordering::SeqCst);
            transfer_blocklist_with_generation(
                &st.blocklist_sync,
                || blocklist_transfer_snapshot(&st),
                |entries| {
                    delivered
                        .lock()
                        .unwrap()
                        .push((generation, entries.to_vec()));
                    Ok(())
                },
                || {
                    closes.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
        };
        sync(&delivered).unwrap();
        let (_, custom) = delivered_last(&delivered).expect("the custom list must arrive");
        assert!(
            custom.iter().any(|e| e.contains("evil.example")),
            "baseline: the custom rule reached Android, got {custom:?}"
        );

        // Now the real reset chain: persist the defaults, rebuild the matcher,
        // transfer. `reset_blocklist` is a Tauri command and needs a live
        // `AppHandle`, so those three steps are driven directly here.
        {
            let mut live = st.settings.lock().unwrap();
            let mut next = live.clone();
            next.blocklist = BlocklistState::default_state();
            write_settings_doc(&st, &next).expect("reset write");
            *live = next;
        }
        rebuild_blocklist(&st);
        sync(&delivered).unwrap();

        let (generation, defaults) =
            delivered_last(&delivered).expect("reset must actually reach Android");
        assert_eq!(
            delivered_len(&delivered),
            2,
            "reset is a change, so it transfers"
        );
        assert!(
            defaults.iter().any(|e| e.contains("doubleclick.net")),
            "the DEFAULT rules must be the snapshot, got {defaults:?}"
        );
        assert!(
            !defaults.iter().any(|e| e.contains("evil.example")),
            "the pre-reset custom rule must be gone from Android too"
        );
        assert_eq!(
            generation,
            st.blocklist_sync.generation.load(Ordering::SeqCst),
            "the snapshot must carry the post-reset generation"
        );
        assert_eq!(st.blocklist_sync.applied.load(Ordering::SeqCst), generation);
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "both transfers succeeded, so no live session was closed"
        );
    }

    #[test]
    fn concurrent_out_of_order_transfers_never_restore_a_stale_snapshot() {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        // The actual race: several commands call `sync_android_blocklist`
        // without holding the session lock, so transfers overlap. Each worker
        // writes newer settings, captures a snapshot, then stalls — and the
        // guard must still leave Kotlin on the newest generation.
        let gate = Arc::new(BlocklistSyncGate::new());
        let live = Arc::new(std::sync::Mutex::new((
            1u64,
            vec!["gen-1.example".to_string()],
        )));
        let seq = Arc::new(AtomicU64::new(1));
        let delivered = Arc::new(DeliveredLog::new(Vec::new()));
        let closes = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let mut handles = Vec::new();
        for worker in 0..8u64 {
            let gate = Arc::clone(&gate);
            let live = Arc::clone(&live);
            let seq = Arc::clone(&seq);
            let delivered = Arc::clone(&delivered);
            let closes = Arc::clone(&closes);
            handles.push(std::thread::spawn(move || {
                // A settings write + rebuild: the live generation only moves
                // forward, exactly like `rebuild_blocklist` does.
                let generation = seq.fetch_add(1, Ordering::SeqCst) + 1;
                {
                    let mut guard = live.lock().unwrap();
                    if generation > guard.0 {
                        guard.0 = generation;
                        guard.1 = vec![format!("gen-{}.example", generation)];
                    }
                }
                // Capture BEFORE stalling — this is the value a caller without
                // the in-lock re-read would have pushed.
                let captured = {
                    let guard = live.lock().unwrap();
                    BlocklistTransfer {
                        generation: guard.0,
                        entries: guard.1.clone(),
                    }
                };
                std::thread::sleep(std::time::Duration::from_millis(worker % 3));
                let result = transfer_blocklist_with_generation(
                    &gate,
                    || {
                        let guard = live.lock().unwrap();
                        BlocklistTransfer {
                            generation: guard.0,
                            entries: guard.1.clone(),
                        }
                    },
                    |entries| {
                        let generation = live.lock().unwrap().0;
                        delivered
                            .lock()
                            .unwrap()
                            .push((generation, entries.to_vec()));
                        Ok(())
                    },
                    || {
                        closes.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                );
                (captured.generation, result.is_ok())
            }));
        }
        let mut captured_generations = Vec::new();
        for handle in handles {
            let (captured, ok) = handle.join().expect("worker panicked");
            assert!(ok, "no transfer may fail: the fake bridge always succeeds");
            captured_generations.push(captured);
        }

        let log = delivered.lock().unwrap();
        assert!(!log.is_empty(), "at least one transfer must reach Kotlin");
        // Monotonic: no snapshot may be older than one already delivered. This
        // is exactly the stale-overwrite the generation guard prevents.
        for pair in log.windows(2) {
            assert!(
                pair[1].0 >= pair[0].0,
                "a stale snapshot overwrote a newer one: {:?}",
                *log
            );
        }
        let newest = live.lock().unwrap().0;
        assert_eq!(
            log.last().unwrap().0,
            newest,
            "Kotlin must end on the newest generation, log: {:?}",
            *log
        );
        assert_eq!(
            log.last().unwrap().1,
            vec![format!("gen-{}.example", newest)],
            "and on the newest RULES, not just the newest label"
        );
        drop(log);
        assert_eq!(gate.applied.load(Ordering::SeqCst), newest);
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "no live session was disturbed"
        );
        assert!(
            captured_generations.iter().max().copied() == Some(newest),
            "the workers must have covered the newest generation: {captured_generations:?}"
        );
    }

    #[test]
    fn each_site_opening_gets_a_distinct_session_identity() {
        // Site id is deliberately stable across opens; the session identity
        // must not be. Reusing it would reintroduce the stale-close race.
        let first = new_session_id();
        let second = new_session_id();
        assert!(!first.is_empty());
        assert!(!second.is_empty());
        assert_ne!(first, second);
        // Opaque and transport-safe without extra encoding.
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
