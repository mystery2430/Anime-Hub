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
use crate::web::session::{build_init_script, CookieJar, InjectedConfig};
use crate::web::windows;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

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
    /// whose cookies are currently loaded into the single shared WebView, so
    /// `open_site` knows which jar to export before swapping.
    pub current_site: Mutex<Option<CurrentSite>>,
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

fn persist_settings(state: &AppState) -> AppResult<()> {
    let s = state
        .settings
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    let bytes = serde_json::to_vec(&*s)?;
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

#[tauri::command]
pub fn remove_site(state: State<'_, AppState>, id: String) -> AppResult<()> {
    {
        let mut reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        reg.remove(&id)?;
    }
    persist_registry(&state)?;
    // Delete the jar before reporting success. A swallowed failure would
    // leave the site's cookies on disk after the tile is gone.
    state.provider.delete_secret(&format!("cookies-{id}"))?;
    Ok(())
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
/// Android: the main WebView is navigated, with the cookie jar swapped first.
#[tauri::command]
pub async fn open_site(app: AppHandle, id: String) -> AppResult<OpenedSite> {
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
    let dns = tauri::async_runtime::spawn_blocking(move || crate::web::dns::classify_host(&host))
        .await
        .map_err(|e| AppError::Other(format!("adres çözümlenemedi: {e}")))?;
    if matches!(dns, crate::web::dns::DnsClass::Answered { private: true }) {
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
        isolated: !cfg!(any(target_os = "android", target_os = "ios")),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedSite {
    /// Window label (desktop) or `"main"` (mobile).
    pub label: String,
    pub mobile: bool,
    /// Whether this platform gives true per-site storage isolation.
    /// `false` on Android — see `web::session` for why.
    pub isolated: bool,
}

/// Close a site session and persist its cookie jar (mobile cookie-swap path).
#[tauri::command]
pub async fn close_site(app: AppHandle, label: String) -> AppResult<()> {
    windows::close_site_window(&app, &label)
}

/// Wipe stored cookies for one site, or for all sites when `id` is `None`.
#[tauri::command]
pub async fn clear_site_data(app: AppHandle, id: Option<String>) -> AppResult<u32> {
    let state = app.state::<AppState>();
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
    let mut n = 0;
    for id in &ids {
        state.provider.delete_secret(&format!("cookies-{id}"))?;
        n += 1;
    }
    // Desktop profile directories hold the real cookie DB. A missing
    // directory is success; a failed delete is not.
    let base = app.path().app_data_dir().map_err(|e| {
        log::error!("profil dizini bulunamadı: {e}");
        AppError::Storage("profil dizini bulunamadı".into())
    })?;
    for id in &ids {
        let dir = base
            .join("profiles")
            .join(crate::web::session::profile_dir_name(id));
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                log::error!("profil dizini silinemedi: {e}");
                return Err(AppError::Storage("site verisi silinemedi".into()));
            }
        }
    }
    Ok(n)
}

/// Read a site's stored cookie jar (used by the mobile swap and by tests).
#[tauri::command]
pub fn get_site_cookies(state: State<'_, AppState>, id: String) -> AppResult<CookieJar> {
    match state.provider.get_secret(&format!("cookies-{id}"))? {
        Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
        None => Ok(CookieJar {
            site_id: id,
            cookies: vec![],
            saved_at: 0,
        }),
    }
}

/// Replace a site's stored cookie jar.
#[tauri::command]
pub fn set_site_cookies(state: State<'_, AppState>, jar: CookieJar) -> AppResult<()> {
    let bytes = serde_json::to_vec(&jar)?;
    state
        .provider
        .put_secret(&format!("cookies-{}", jar.site_id), &bytes)
}

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

#[tauri::command]
pub fn update_settings(
    state: State<'_, AppState>,
    patch: SettingsPatch,
) -> AppResult<SettingsView> {
    {
        let mut s = state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        if let Some(b) = patch.blocklist {
            s.blocklist = b;
        }
        if let Some(v) = patch.block_popups {
            s.block_popups = v;
        }
        if let Some(v) = patch.inject_cosmetic_rules {
            s.inject_cosmetic_rules = v;
        }
        if let Some(v) = patch.fullscreen_sites {
            s.fullscreen_sites = v;
        }
        if let Some(v) = patch.theme {
            s.theme = v;
        }
        if let Some(v) = patch.pip_auto_enter {
            s.pip_auto_enter = v;
        }
        if let Some(a) = patch.anilist {
            s.anilist = a;
        }
    }
    persist_settings(&state)?;
    rebuild_blocklist(&state);
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
        let mut s = state
            .settings
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        s.blocklist = BlocklistState::default_state();
    }
    persist_settings(&state)?;
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
    let (code, returned_state) = anilist::parse_callback(&callback_url)?;

    let expected = {
        let s = state
            .oauth_state
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        s.clone()
    };
    let expected = expected.ok_or_else(|| AppError::AniList("bekleyen bir oturum yok".into()))?;
    if !anilist::state_matches(&expected, &returned_state) {
        return Err(AppError::AniList("state doğrulaması başarısız".into()));
    }

    let (cli, _) = client(&state)?;
    let token = cli.exchange_code(&code).await?;
    let viewer = cli.viewer(&token).await?;
    save_token(&state, &token)?;
    {
        let mut s = state
            .oauth_state
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        *s = None;
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
                    if let Err(e) = state.provider.delete_secret("anilist-token") {
                        log::error!("süresi dolmuş AniList oturumu silinemedi: {e}");
                    }
                    return Ok(None);
                }
            },
            None => {
                if let Err(e) = state.provider.delete_secret("anilist-token") {
                    log::error!("yenilenemeyen AniList oturumu silinemedi: {e}");
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
    if let Err(e) = state.provider.delete_secret("anilist-token") {
        log::error!("geçersiz AniList oturumu silinemedi: {e}");
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
    window.set_theme(target).map_err(|e| {
        log::error!("tema uygulanamadı: {e}");
        AppError::Other("tema uygulanamadı".into())
    })
}

#[tauri::command]
pub fn enter_pip(num: Option<u32>, den: Option<u32>) -> AppResult<bool> {
    let (n, d) = crate::android_bridge::clamp_aspect(num.unwrap_or(16), den.unwrap_or(9));
    crate::android_bridge::enter_pip(n, d)
}

#[tauri::command]
pub fn set_pip_auto_enter(enabled: bool) -> AppResult<bool> {
    crate::android_bridge::set_pip_auto_enter(enabled)
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
        android_isolation_note: cfg!(target_os = "android"),
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
    /// `true` on Android, where storage isolation is cookie-level only.
    pub android_isolation_note: bool,
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
        }
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

    #[test]
    fn platform_and_webview_names_are_known() {
        assert!(!platform_name().is_empty());
        assert!(!webview_name().is_empty());
        assert_eq!(backend_name(Backend::AndroidKeystore), "Android Keystore");
    }
}
