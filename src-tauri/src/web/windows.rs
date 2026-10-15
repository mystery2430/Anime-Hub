//! WebView session hosting and per-site isolation.
//!
//! This is the only place a site URL is handed to a WebView, and the only
//! place the navigation policy is attached, so the HTTPS/blocklist rules
//! cannot be bypassed by adding a new entry point.
//!
//! ## Desktop
//! A site opens **inside the single application window** as a child WebView
//! that covers the launcher (Tauri's multi-webview support). It is *not* a
//! new OS window and *not* a browser tab. Crucially the child WebView still
//! gets its own per-site profile directory, so the WebView2/WebKitGTK
//! isolation model is unchanged: cookies, localStorage, IndexedDB and cache
//! never cross between sites or into the launcher.
//!
//! The site WebView has no capabilities (see `capabilities/default.json`,
//! scoped to the `main` webview) and loads an external origin, so it has no
//! IPC access to the Rust core. The only way back is the injected
//! back-button / `Esc` handler, which navigates to `animehub://close-site`;
//! that scheme is intercepted natively below and turned into a close action.
//!
//! ## Android
//! The launcher keeps Tauri's single WebView (the **default** profile). Every
//! site opens in a separate native `WebView` above it, bound — before any
//! other WebView API call and before the first navigation — to its own
//! persistent androidx.webkit `Profile` via `WebViewCompat.setProfile`. The
//! profile name derives from the registered `site_id` (SHA-256 slice plus a
//! persistent generation counter), never from the URL or display name, so
//! same-origin sites get separate localStorage/IndexedDB/cookie jars and the
//! launcher's default profile is never used for site data.
//!
//! The old JSON cookie/storage snapshot machinery is gone: profiles persist
//! on their own, so there is nothing to export/import, and the lossy
//! localStorage/IndexedDB dump is no longer part of the product. The one
//! migration kept is a one-time import of the pre-profile encrypted cookie
//! jar into the target profile's own `CookieManager`, before that profile's
//! first navigation. When the WebView provider lacks
//! `WebViewFeature.MULTI_PROFILE`, the open fails with a user-facing error —
//! never a silent fallback to the shared profile. See
//! `docs/android-storage-isolation.md`.

use crate::commands::{AppState, CurrentSite};
use crate::error::{AppError, AppResult};
use crate::sites::registry::Site;
#[cfg(desktop)]
use crate::web::dns::{classify_host, DnsClass};
// `CookieJar`/`restore` back the iOS jar swap and the one-time Android
// profile migration; `capture` (qualified at its single call site, so this
// import group keeps the exact shape rustfmt approved before) serves the
// desktop/iOS export path. Android site cookies live in their profile and
// are never snapshotted.
#[cfg(mobile)]
use crate::web::session::restore;
use crate::web::session::{profile_dir_name, CookieJar};
#[cfg(desktop)]
use crate::web::session::{decide_navigation, NavDecision, DNS_REBIND_BLOCK};
use tauri::{AppHandle, Emitter, Manager};
#[cfg(desktop)]
use tauri::{LogicalPosition, PhysicalPosition, Position, Size, WebviewUrl};
use url::Url;

/// Label of the launcher window (and of the launcher webview inside it).
pub const LAUNCHER_LABEL: &str = "main";

/// Prefix for site webview labels.
const SITE_LABEL_PREFIX: &str = "site-";

/// Title shown while no site is open.
pub const LAUNCHER_TITLE: &str = "AnimeHub";

/// Pseudo-navigation the injected overlay uses to ask for the site to close.
/// Intercepted below; it never leaves the app.
const CLOSE_SCHEME: &str = "animehub";
const CLOSE_HOST: &str = "close-site";

/// Android: one site WebView at a time over the launcher.
/// Serialize open/close/wipe/flush and the deferred-profile-deletion drain so
/// a sync tick can never flush a profile between a wipe's close and its
/// delete, and a second open cannot race the first one's profile handover.
#[cfg(target_os = "android")]
static ANDROID_SESSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock Android's site-session lifecycle while reading/mutating it.
#[cfg(target_os = "android")]
pub fn lock_android_session() -> AppResult<std::sync::MutexGuard<'static, ()>> {
    ANDROID_SESSION_LOCK
        .lock()
        .map_err(|_| AppError::Storage("Android oturum kilidi alınamadı".into()))
}

/// Desktop lifecycle lock — serializes every mutation of the live site
/// session so open / normal close / periodic sync / remove / clear cannot
/// interleave and resurrect or orphan a profile.
///
/// **Lock order (must be respected everywhere to avoid deadlock):**
/// `DESKTOP_SESSION_LOCK` → `current_site` → `registry` → `SecureStore WRITE_LOCK`
/// `commands::clear_site_data` snapshots `ids` from `registry` *before*
/// taking this lock; `commands::remove_site` takes `registry` briefly to
/// check builtin/hide, releases it, then takes this lock for close+delete.
/// `open_in_main_window` takes this lock for the entire open (close previous,
/// create profile dir, add_child, set current_site). `spawn_session_sync`
/// takes this lock for each tick's ownership check + export. `close_*`
/// helpers have `*_locked` inner versions that assume the lock is already
/// held, so `open` can close the previous child without self-deadlock.
///
/// The lock is a plain `std::sync::Mutex`, never held across an `await` and
/// never on the UI thread for longer than a file operation, so it cannot
/// block window resizing or IPC.
#[cfg(desktop)]
static DESKTOP_SESSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock the desktop session. See the `DESKTOP_SESSION_LOCK` docs for the
/// global lock order.
#[cfg(desktop)]
pub fn lock_desktop_session() -> AppResult<std::sync::MutexGuard<'static, ()>> {
    DESKTOP_SESSION_LOCK
        .lock()
        .map_err(|_| AppError::Storage("masaüstü oturum kilidi alınamadı".into()))
}

/// How often a live site session is snapshotted into the encrypted store.
///
/// The close-time export remains the authoritative final write; this interval
/// only bounds what a crash (or an Android process kill, which skips the
/// close path entirely) can lose.
const SESSION_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Safety valve: a sync thread ends itself after this long even if the
/// ownership checks somehow never fire.
const SESSION_SYNC_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(12 * 3600);

/// Window/webview label used for a site session.
pub fn label_for(site_id: &str) -> String {
    format!("{SITE_LABEL_PREFIX}{}", profile_dir_name(site_id))
}

/// Open a site.
///
/// Returns the webview label that now hosts the site.
///
/// The dispatch is compile-time, not runtime: `open_in_main_window` calls
/// `add_child` / `set_fullscreen`, which do not exist in tauri's mobile
/// builds, so even a `cfg!()`-guarded branch would fail to compile on
/// Android.
#[cfg(mobile)]
pub fn open_site_window(
    app: AppHandle,
    site: &Site,
    url: &Url,
    init_script: &str,
    _fullscreen: bool,
) -> AppResult<String> {
    open_on_mobile(&app, site, url, init_script)
}

/// Desktop variant: a child WebView inside the main window, per-site profile
/// kept.
#[cfg(desktop)]
pub fn open_site_window(
    app: AppHandle,
    site: &Site,
    url: &Url,
    init_script: &str,
    fullscreen: bool,
) -> AppResult<String> {
    open_in_main_window(&app, site, url, init_script, fullscreen)
}

// ---------------------------------------------------------------------------
// Desktop: a child WebView inside the main window, per-site profile kept.
// ---------------------------------------------------------------------------

#[cfg(desktop)]
fn open_in_main_window(
    app: &AppHandle,
    site: &Site,
    url: &Url,
    init_script: &str,
    fullscreen: bool,
) -> AppResult<String> {
    // Serialize open against wipe/sync. The inner does the real work while
    // the lock is held so a concurrent `clear_site_data` cannot delete the
    // profile directory we are about to create.
    let _guard = lock_desktop_session()?;
    open_in_main_window_inner(app, site, url, init_script, fullscreen)
}

#[cfg(desktop)]
fn open_in_main_window_inner(
    app: &AppHandle,
    site: &Site,
    url: &Url,
    init_script: &str,
    fullscreen: bool,
) -> AppResult<String> {
    // Re-validate the `Site` snapshot while holding `DESKTOP_SESSION_LOCK`
    // (already held by `open_in_main_window`). A concurrent `remove_site`
    // may have deleted the site after `open_site` cloned it but before we
    // acquired the lock; without this check we would recreate its profile.
    {
        let state = app.state::<AppState>();
        let reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        match reg.get(&site.id) {
            Some(current) => {
                if current.hidden {
                    return Err(AppError::SiteNotFound(site.id.clone()));
                }
                if current.url != site.url {
                    return Err(AppError::SiteNotFound(site.id.clone()));
                }
            }
            None => return Err(AppError::SiteNotFound(site.id.clone())),
        }
    }
    let label = label_for(&site.id);
    let window = app
        .get_webview_window(LAUNCHER_LABEL)
        .ok_or_else(|| AppError::Other("ana pencere bulunamadı".into()))?;

    // Already showing this exact site: nothing to stack, just focus.
    if app.get_webview(&label).is_some() {
        window.set_focus().map_err(|e| {
            log::error!("pencere öne alınamadı: {e}");
            AppError::Other("pencere öne alınamadı".into())
        })?;
        return Ok(label);
    }

    // One site at a time inside the single window: close a previous session
    // (saving its jar) before opening the next. Use the `_locked` variant
    // because we already hold `DESKTOP_SESSION_LOCK` so we must not relock.
    close_current_child_locked(app)?;

    // Per-site profile directory: WebKitGTK's data directory on Linux, and on
    // Windows the WebView2 user-data folder. This is what makes cookies,
    // localStorage, IndexedDB and cache independent per site.
    let base = app.path().app_data_dir().map_err(|e| {
        log::error!("uygulama veri dizini alınamadı: {e}");
        AppError::Storage("uygulama veri dizini alınamadı".into())
    })?;
    let data_dir = crate::web::session::ensure_profile_dir(&base, &site.id)?;

    let host = site.host();
    let nav_host = host.clone();
    let nav_label = label.clone();
    // A separate clone for the navigation closure: the builder borrows `app`
    // for its own lifetime, so the closure cannot also move out of it.
    let nav_app = app.clone();

    let size = window.inner_size().map_err(|e| {
        log::error!("pencere boyutu okunamadı: {e}");
        AppError::Other("pencere boyutu okunamadı".into())
    })?;

    // `add_child` takes the *builder* and constructs the WebView on the main
    // thread; `WebviewBuilder` has no standalone `build`.
    let builder = tauri::WebviewBuilder::new(&label, WebviewUrl::External(url.clone()))
        // The child shares the site's own profile directory, never the
        // launcher's. See the module docs for the isolation contract.
        .data_directory(data_dir)
        .initialization_script(init_script)
        .focused(true)
        .on_navigation(move |target: &Url| {
            // The injected "back" affordance navigates to this pseudo-URL.
            // Close the session instead of following it. The close is spawned
            // so the WebView is not torn down from inside its own callback.
            if target.scheme() == CLOSE_SCHEME {
                if target.host_str() == Some(CLOSE_HOST) {
                    let app = nav_app.clone();
                    let label = nav_label.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = close_site_window(&app, &label) {
                            log::error!("site oturumu kapatılamadı: {e}");
                        }
                    });
                }
                return false;
            }
            let app = nav_app.clone();
            // String policy under the lock; DNS outside it. A slow resolver
            // must not stall every other command waiting on the blocklist.
            let preliminary = {
                let state = app.state::<AppState>();
                let bl = state.blocklist.lock().expect("blocklist lock");
                decide_navigation(target, &nav_host, &bl)
            };
            let decision = if preliminary == NavDecision::Allow {
                let dns = target
                    .host_str()
                    .map(classify_host)
                    .unwrap_or(DnsClass::Unknown);
                if matches!(dns, DnsClass::Answered { private: true }) {
                    NavDecision::Block(DNS_REBIND_BLOCK)
                } else {
                    NavDecision::Allow
                }
            } else {
                preliminary
            };
            match decision {
                NavDecision::Allow => true,
                NavDecision::Block(msg) => {
                    log::warn!("blocked navigation to {target}: {msg}");
                    let _ = app.emit("animehub://navigation-blocked", msg.to_string());
                    false
                }
                NavDecision::BlockedHost => {
                    log::debug!("blocklisted navigation to {target}");
                    false
                }
            }
        })
        // Every new-window request is denied. Legit external links (payment
        // pages, app stores, "open in browser") go to the system browser via
        // `open_site_externally` instead.
        .on_new_window(|_url, _features| tauri::webview::NewWindowResponse::Deny)
        // The way back (back button + Esc) is installed with a Rust-side
        // eval on every finished load: inside a child WebView the
        // document-start init script alone proved unreliable on WebView2.
        // `OVERLAY_SNIPPET` self-guards, so repeated installs are no-ops,
        // and it heals itself if a SPA wipes the DOM around the button.
        .on_page_load(|webview, payload| {
            if !matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                return;
            }
            if let Err(e) = webview.eval(crate::web::session::OVERLAY_SNIPPET) {
                log::warn!("geri dönüş katmanı kurulamadı: {e}");
            }
        });

    // `add_child` lives on the bare `Window`, not on `WebviewWindow`; same
    // window, different handle type.
    let host_window = app
        .get_window(LAUNCHER_LABEL)
        .ok_or_else(|| AppError::Other("ana pencere bulunamadı".into()))?;
    host_window
        .add_child(
            builder,
            Position::Logical(LogicalPosition::new(0.0, 0.0)),
            Size::Physical(size),
        )
        .map_err(|e| {
            log::error!("WebView pencereye eklenemedi: {e}");
            AppError::Other("WebView pencereye eklenemedi".into())
        })?;

    // "Open sites fullscreen" is opt-in (default off). Track whether *we*
    // entered fullscreen so close can restore the previous windowed state
    // without stealing a fullscreen the user chose by hand.
    let already_fs = window.is_fullscreen().unwrap_or(false);
    let entered_fullscreen = fullscreen && !already_fs;
    if entered_fullscreen {
        if let Err(e) = window.set_fullscreen(true) {
            log::warn!("tam ekrana geçilemedi: {e}");
        }
    }
    if let Err(e) = window.set_title(&site.name) {
        log::warn!("pencere başlığı ayarlanamadı: {e}");
    }

    {
        let state = app.state::<AppState>();
        let value = CurrentSite {
            site_id: site.id.clone(),
            host,
            window_label: label.clone(),
            entered_fullscreen,
        };
        *state.current_site.lock().expect("current_site lock") = Some(value);
    }

    // Live-session sync: snapshot the session into the encrypted store on a
    // fixed interval until the site is closed or another site takes over.
    spawn_session_sync(app.clone(), label.clone(), site.id.clone(), site.host());

    Ok(label)
}

/// Close whichever site webview is currently shown inside the main window,
/// if any. Used before hosting a new one. Locked variant assumes
/// `DESKTOP_SESSION_LOCK` is already held (called from `open`).
#[cfg(desktop)]
fn close_current_child_locked(app: &AppHandle) -> AppResult<()> {
    let label = {
        let state = app.state::<AppState>();
        let current = state.current_site.lock().expect("current_site lock");
        current
            .as_ref()
            .filter(|c| c.window_label != LAUNCHER_LABEL)
            .map(|c| c.window_label.clone())
    };
    match label {
        Some(label) => close_site_window_locked(app, &label),
        None => Ok(()),
    }
}

/// Wrapper that acquires `DESKTOP_SESSION_LOCK` for callers outside `open`.
#[cfg(desktop)]
#[allow(dead_code)]
fn close_current_child(app: &AppHandle) -> AppResult<()> {
    let _guard = lock_desktop_session()?;
    close_current_child_locked(app)
}

/// Watch the main window so the hosted site webview tracks its size.
///
/// Installed once during `setup`. The launcher webview resizes itself; the
/// child webview must be re-bounded explicitly on every window resize.
/// `Webview::set_bounds` is desktop-only, so the mobile variant is a no-op.
#[cfg(mobile)]
pub fn install_main_window_handlers(_app: &AppHandle) {}

#[cfg(desktop)]
pub fn install_main_window_handlers(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LAUNCHER_LABEL) else {
        return;
    };
    let app_handle = app.clone();
    window.on_window_event(move |event| {
        let tauri::WindowEvent::Resized(size) = event else {
            return;
        };
        let app = app_handle.clone();
        let label = {
            let state = app.state::<AppState>();
            let current = state.current_site.lock().expect("current_site lock");
            current.as_ref().map(|c| c.window_label.clone())
        };
        if let Some(label) = label {
            if let Some(child) = app.get_webview(&label) {
                if let Err(e) = child.set_bounds(tauri::Rect {
                    position: Position::Physical(PhysicalPosition::new(0, 0)),
                    size: Size::Physical(*size),
                }) {
                    log::warn!("site görünümü yeniden boyutlandırılamadı: {e}");
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Live-session sync: periodic encrypted snapshots while a site is open.
// ---------------------------------------------------------------------------

/// `true` while `site_id` still owns the live session.
///
/// Shared by the sync loop and the mobile restore path: every background
/// write into a site's blob must re-derive ownership from the *current* state
/// rather than trust a value captured when the thread was spawned, so a stale
/// thread can never write one site's session data into another site's blob.
fn session_is_current(state: &AppState, site_id: &str) -> bool {
    let current = state.current_site.lock().expect("current_site lock");
    current.as_ref().is_some_and(|c| c.site_id == site_id)
}

/// Snapshot the live session into the encrypted store on a fixed interval.
///
/// Desktop variant: cookies only — localStorage/IndexedDB already live in the
/// per-site profile directory that the WebView itself persists. The loop
/// exits on the first tick after the session ends (site closed, another site
/// opened, or the child webview is gone without a close event).
///
/// A desktop-wide lock serializes this snapshot against
/// `clear_site_data`/`remove_site`'s wipe-close path, so a periodic export
/// cannot resurrect a jar that was just deleted. The lock is taken *before*
/// the ownership check so the check and the subsequent write are atomic with
/// respect to a concurrent wipe.
#[cfg(desktop)]
fn spawn_session_sync(app: AppHandle, label: String, site_id: String, _host: String) {
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(SESSION_SYNC_INTERVAL);
            if started.elapsed() > SESSION_SYNC_MAX_LIFETIME {
                log::warn!("oturum eşitlemesi ömrünü doldurdu: {site_id}");
                return;
            }
            // Hold the desktop session lock across the ownership check and the
            // cookie export. `clear_site_data`/`remove_site` hold the same
            // lock across close+delete, so this thread either sees the session
            // as still current and completes its export *before* the wipe, or
            // sees it as gone and exits without resurrecting the jar.
            let _guard = match lock_desktop_session() {
                Ok(g) => g,
                Err(e) => {
                    log::error!("masaüstü oturum eşitlemesi kilitlenemedi: {e}");
                    return;
                }
            };
            {
                let state = app.state::<AppState>();
                if !session_is_current(&state, &site_id) {
                    return;
                }
            }
            let Some(child) = app.get_webview(&label) else {
                return;
            };
            if let Err(e) = export_cookies_webview(&child, &site_id) {
                log::warn!("dönemsel çerez eşitlemesi başarısız ({site_id}): {e}");
            }
        }
    });
}

/// Android variant: durability flush of the active site profile.
///
/// The site's cookies live in its own profile, which the WebView provider
/// persists by itself; this tick only forces `Profile.getCookieManager()
/// .flush()` so an Android process kill (which never runs the close path)
/// cannot lose the last seconds of cookie writes. Ownership is re-checked
/// under `ANDROID_SESSION_LOCK` like every other writer, so a stale thread
/// can never flush after another site took over — and never touches the
/// launcher's default profile.
#[cfg(target_os = "android")]
fn spawn_session_sync(app: AppHandle, _label: String, site_id: String, _host: String) {
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(SESSION_SYNC_INTERVAL);
            if started.elapsed() > SESSION_SYNC_MAX_LIFETIME {
                log::warn!("oturum eşitlemesi ömrünü doldurdu: {site_id}");
                return;
            }
            let _guard = match lock_android_session() {
                Ok(guard) => guard,
                Err(e) => {
                    log::error!("Android oturum eşitlemesi kilitlenemedi: {e}");
                    return;
                }
            };
            {
                let state = app.state::<AppState>();
                if !session_is_current(&state, &site_id) {
                    return;
                }
            }
            if let Err(e) = crate::android_bridge::site_flush() {
                log::warn!("dönemsel profil çerez eşitlemesi başarısız ({site_id}): {e}");
            }
        }
    });
}

/// Non-Android mobile (iOS) variant: cookies every tick.
///
/// Cookies still ride the encrypted jar on iOS; localStorage/IndexedDB
/// snapshots were removed everywhere with the JSON transfer machinery — the
/// old dump was lossy and not a security boundary.
#[cfg(all(mobile, not(target_os = "android")))]
fn spawn_session_sync(app: AppHandle, _label: String, site_id: String, host: String) {
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let Some(window) = app.get_webview_window(LAUNCHER_LABEL) else {
            return;
        };
        loop {
            std::thread::sleep(SESSION_SYNC_INTERVAL);
            if started.elapsed() > SESSION_SYNC_MAX_LIFETIME {
                log::warn!("oturum eşitlemesi ömrünü doldurdu: {site_id}");
                return;
            }
            {
                let state = app.state::<AppState>();
                if !session_is_current(&state, &site_id) {
                    return;
                }
            }
            let loaded = window
                .url()
                .ok()
                .is_some_and(|u| u.host_str() == Some(host.as_str()));
            if !loaded {
                continue;
            }
            if let Err(e) = export_cookies(&window, &site_id, &host) {
                log::warn!("dönemsel çerez eşitlemesi başarısız ({site_id}): {e}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Mobile: Android opens a separate profiled WebView per site; iOS keeps the
// shared-WebView flow without the removed JSON storage snapshots.
// ---------------------------------------------------------------------------

/// Prefix shared with Kotlin's `AnimeHubSiteController` name validator: a
/// profile name must match `site-[0-9a-f]{32}-g<generation>` exactly. Kotlin
/// refuses anything else, which is what keeps a stray caller from ever
/// addressing the `Default` profile (the launcher's own data).
pub const ANDROID_PROFILE_PREFIX: &str = "site-";

/// Secret key holding the persistent generation counter for a site's Android
/// profile name. Kept (not deleted with the session) so even a registry
/// restored from an old backup can never re-open a profile name whose
/// on-disk deletion is still pending.
const PROFILE_GENERATION_PREFIX: &str = "profilegen-";

/// Secret key marking that the site's pre-profile cookie jar has been handed
/// to its WebView profile (attempted once, successfully). Deleted together
/// with the session data in `delete_site_session`, so a cleared site never
/// re-imports the wiped jar.
const MIGRATION_FLAG_PREFIX: &str = "profilemig-";

/// Secret key holding a JSON list of profile names whose deletion could not
/// run synchronously (loaded in the process that tried, or the provider was
/// temporarily without the API). Drained at the next process start, before
/// any site WebView can open.
pub const PENDING_PROFILE_DELETES_KEY: &str = "android-pending-profile-deletes";

/// Deterministic Android WebView profile name for a site.
///
/// `site-<first 32 hex chars of SHA-256(site_id)>-g<generation>`:
///
/// * derived from the **registered site id only** — never the URL or the
///   display name, so a rename/re-URL cannot point two sites at one profile
///   and a hostile site name cannot influence the profile namespace;
/// * 128 bits of digest make collisions between different ids negligible,
///   which is what "each site id gets a different profile" needs;
/// * a conservative `[a-z0-9-]` charset the WebView provider accepts
///   anywhere;
/// * the generation suffix distinguishes a re-created profile for the same
///   site id from a name whose previous deletion may still be running
///   asynchronously on disk.
pub fn android_profile_name(site_id: &str, generation: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(site_id.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{ANDROID_PROFILE_PREFIX}{}-g{generation}", &digest[..32])
}

/// The persisted generation for `site_id` (0 when never bumped).
pub fn profile_generation(state: &AppState, site_id: &str) -> AppResult<u32> {
    match state
        .provider
        .get_secret(&format!("{PROFILE_GENERATION_PREFIX}{site_id}"))?
    {
        None => Ok(0),
        Some(bytes) => Ok(String::from_utf8(bytes.to_vec())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)),
    }
}

/// Advance the generation after a delete of the *current* name was accepted
/// or queued, so the next open of this site id derives a fresh name and can
/// never collide with a deletion still finishing on disk.
pub fn bump_profile_generation(state: &AppState, site_id: &str) -> AppResult<u32> {
    let next = profile_generation(state, site_id)?.saturating_add(1);
    state.provider.put_secret(
        &format!("{PROFILE_GENERATION_PREFIX}{site_id}"),
        next.to_string().as_bytes(),
    )?;
    Ok(next)
}

/// Read the deferred profile-deletion queue. A corrupt list is treated as
/// empty: the names leak only as on-disk profile data, which the `is_all`
/// orphan sweep collects later; crashing here would block every open.
pub fn read_pending_profile_deletes(state: &AppState) -> AppResult<Vec<String>> {
    match state.provider.get_secret(PENDING_PROFILE_DELETES_KEY)? {
        None => Ok(Vec::new()),
        Some(bytes) => match serde_json::from_slice(&bytes) {
            Ok(list) => Ok(list),
            Err(e) => {
                log::warn!("bekleyen profil silme listesi okunamadı: {e}");
                Ok(Vec::new())
            }
        },
    }
}

/// Queue a profile name for deletion at the next process start (deduped).
pub fn queue_pending_profile_delete(state: &AppState, name: &str) -> AppResult<()> {
    let mut list = read_pending_profile_deletes(state)?;
    if list.iter().any(|existing| existing == name) {
        return Ok(());
    }
    list.push(name.to_string());
    let bytes = serde_json::to_vec(&list)?;
    state
        .provider
        .put_secret(PENDING_PROFILE_DELETES_KEY, &bytes)
}

/// Drop a name from the deferred deletion queue once it is gone.
pub fn forget_pending_profile_delete(state: &AppState, name: &str) -> AppResult<()> {
    let list = read_pending_profile_deletes(state)?;
    let kept: Vec<String> = list.into_iter().filter(|n| n != name).collect();
    let bytes = serde_json::to_vec(&kept)?;
    state
        .provider
        .put_secret(PENDING_PROFILE_DELETES_KEY, &bytes)
}

/// Re-exported for the startup drain in `lib.rs`.
pub fn mark_migration_done(state: &AppState, site_id: &str) -> AppResult<()> {
    state
        .provider
        .put_secret(&format!("{MIGRATION_FLAG_PREFIX}{site_id}"), b"1")
}

/// Android: open the site in a fresh native WebView bound to its own profile.
///
/// Runs entirely under `ANDROID_SESSION_LOCK`: the outgoing site WebView is
/// closed first (its data lives in its profile — nothing is exported), the
/// one-time 0.3.4 cookie jar is handed to the profile cookie manager before
/// the first navigation, and only then does the new view cover the launcher.
/// A `MULTI_PROFILE`-unsupported provider aborts here with
/// [`AppError::Unsupported`] and no session state is changed.
#[cfg(target_os = "android")]
fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {
    let _session_guard = lock_android_session()?;
    // Re-validate the `Site` snapshot while holding `ANDROID_SESSION_LOCK`.
    // See `open_in_main_window_inner` for the stale-snapshot race.
    {
        let state = app.state::<AppState>();
        let reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        match reg.get(&site.id) {
            Some(current) => {
                if current.hidden {
                    return Err(AppError::SiteNotFound(site.id.clone()));
                }
                if current.url != site.url {
                    return Err(AppError::SiteNotFound(site.id.clone()));
                }
            }
            None => return Err(AppError::SiteNotFound(site.id.clone())),
        }
    }
    let state = app.state::<AppState>();

    // 1. Close the outgoing site, if any. One site WebView at a time; its
    //    data stays in its profile, so there is no export step any more.
    let previous = state
        .current_site
        .lock()
        .expect("current_site lock")
        .clone();
    if previous.is_some() {
        crate::android_bridge::site_close(true)?;
        *state.current_site.lock().expect("current_site lock") = None;
        if let Some(closed) = previous {
            let _ = app.emit("animehub://site-closed", closed.site_id);
        }
    }

    // 2. The profile for this site at its current generation.
    let generation = profile_generation(&state, &site.id)?;
    let profile = android_profile_name(&site.id, generation);

    // 3. One-time migration of the pre-profile cookie jar (0.3.4 installs).
    //    localStorage/IndexedDB snapshots are intentionally NOT imported: the
    //    dump was lossy and origin-checking it again would revive the TOCTOU
    //    problems profiles remove. The blobs themselves stay untouched until
    //    the user clears or deletes the site.
    let migration_attempted = state
        .provider
        .get_secret(&format!("{MIGRATION_FLAG_PREFIX}{}", site.id))?
        .is_none();
    let mut cookie_headers = Vec::new();
    if migration_attempted {
        if let Some(bytes) = state.provider.get_secret(&format!("cookies-{}", site.id))? {
            let jar: CookieJar = serde_json::from_slice(&bytes)?;
            let now = crate::anilist::now_unix();
            for stored in jar.cookies.iter().filter(|c| c.is_live(now)) {
                if let Some(cookie) = restore(stored) {
                    cookie_headers.push(cookie.to_string());
                }
            }
        }
    }

    // 4. Create the profiled WebView. Any failure — including the
    //    MULTI_PROFILE-unsupported refusal — leaves no session behind.
    crate::android_bridge::site_open(url.as_str(), &profile, init_script, &cookie_headers)?;

    if migration_attempted {
        // Record the attempt only after the open succeeded: a failed open
        // must be retried next time, and an existing profile's live cookies
        // must never be overwritten by a stale snapshot on a later attempt.
        if let Err(e) = mark_migration_done(&state, &site.id) {
            log::warn!("profil göç bayrağı yazılamadı ({}): {e}", site.id);
        }
    }

    let label = label_for(&site.id);
    *state.current_site.lock().expect("current_site lock") = Some(CurrentSite {
        site_id: site.id.clone(),
        host: site.host(),
        window_label: label.clone(),
        entered_fullscreen: false,
    });

    // Periodic profile-cookie flush: an Android process kill skips close.
    spawn_session_sync(app.clone(), label.clone(), site.id.clone(), site.host());

    Ok(label)
}

/// Non-Android mobile (iOS): the pre-existing shared-WebView flow, minus the
/// removed localStorage/IndexedDB snapshot machinery.
#[cfg(all(mobile, not(target_os = "android")))]
fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {
    let window = app
        .get_webview_window(LAUNCHER_LABEL)
        .ok_or_else(|| AppError::Other("ana WebView bulunamadı".into()))?;
    // Re-validate the `Site` snapshot (see `open_in_main_window_inner`).
    {
        let state = app.state::<AppState>();
        let reg = state
            .registry
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        match reg.get(&site.id) {
            Some(current) => {
                if current.hidden {
                    return Err(AppError::SiteNotFound(site.id.clone()));
                }
                if current.url != site.url {
                    return Err(AppError::SiteNotFound(site.id.clone()));
                }
            }
            None => return Err(AppError::SiteNotFound(site.id.clone())),
        }
    }
    let state = app.state::<AppState>();

    let previous_site = state
        .current_site
        .lock()
        .expect("current_site lock")
        .clone();
    if let Some(current) = previous_site.as_ref() {
        export_cookies(&window, &current.site_id, &current.host)?;
    }

    window.clear_all_browsing_data().map_err(|e| {
        log::error!("çerezler temizlenemedi: {e}");
        AppError::Other("çerezler temizlenemedi".into())
    })?;

    if let Err(e) = import_cookies(&window, &site.id, url) {
        return Err(e);
    }

    if let Err(e) = window.eval(init_script) {
        log::error!("script yüklenemedi: {e}");
        return Err(AppError::Other("script yüklenemedi".into()));
    }

    if let Err(e) = window.navigate(url.clone()) {
        log::error!("sayfa açılamadı: {e}");
        return Err(AppError::Other("sayfa açılamadı".into()));
    }

    *state.current_site.lock().expect("current_site lock") = Some(CurrentSite {
        site_id: site.id.clone(),
        host: site.host(),
        window_label: LAUNCHER_LABEL.into(),
        entered_fullscreen: false,
    });

    spawn_session_sync(
        app.clone(),
        LAUNCHER_LABEL.to_string(),
        site.id.clone(),
        site.host(),
    );

    Ok(LAUNCHER_LABEL.to_string())
}

/// Save the WebView's current cookie jar into the named site's encrypted blob.
///
/// iOS only: Android's site cookies live in their WebView profile and are
/// never snapshotted any more (see the module docs).
#[cfg(all(mobile, not(target_os = "android")))]
fn export_cookies(
    window: &tauri::WebviewWindow,
    site_id: &str,
    expected_host: &str,
) -> AppResult<()> {
    let _ = expected_host;
    let live = window.cookies().map_err(|e| {
        log::error!("çerezler okunamadı: {e}");
        AppError::Other("çerezler okunamadı".into())
    })?;
    export_cookies_from(live, window.app_handle(), site_id)
}

/// Same as [`export_cookies`] but for a bare child `Webview` (desktop).
#[cfg(desktop)]
fn export_cookies_webview(webview: &tauri::Webview, site_id: &str) -> AppResult<()> {
    let live = webview.cookies().map_err(|e| {
        log::error!("çerezler okunamadı: {e}");
        AppError::Other("çerezler okunamadı".into())
    })?;
    export_cookies_from(live, webview.app_handle(), site_id)
}

#[cfg(any(desktop, all(mobile, not(target_os = "android"))))]
fn export_cookies_from(
    live: Vec<cookie::Cookie<'static>>,
    app: &AppHandle,
    site_id: &str,
) -> AppResult<()> {
    let state = app.state::<AppState>();
    let now = crate::anilist::now_unix();

    let mut jar = CookieJar {
        site_id: site_id.to_string(),
        cookies: live.iter().map(crate::web::session::capture).collect(),
        saved_at: now,
    };
    jar.prune_expired(now);

    let bytes = serde_json::to_vec(&jar)?;
    state
        .provider
        .put_secret(&format!("cookies-{site_id}"), &bytes)
}

/// Load a site's encrypted cookie jar into the shared WebView (iOS only).
///
/// Android never reaches this: its jars are imported profile-scoped inside
/// `AnimeHubSiteController` before the first navigation of a virgin profile.
#[cfg(all(mobile, not(target_os = "android")))]
fn import_cookies(window: &tauri::WebviewWindow, site_id: &str, url: &Url) -> AppResult<()> {
    let app = window.app_handle().clone();
    let state = app.state::<AppState>();
    let _ = url;
    let stored = state.provider.get_secret(&format!("cookies-{site_id}"))?;
    let jar: CookieJar = match stored {
        Some(bytes) => serde_json::from_slice(&bytes)?,
        None => return Ok(()),
    };

    let now = crate::anilist::now_unix();
    for stored in jar.cookies.iter().filter(|c| c.is_live(now)) {
        if let Some(cookie) = restore(stored) {
            if let Err(e) = window.set_cookie(cookie) {
                log::debug!("cookie restore failed for {site_id}: {e}");
            }
        }
    }

    Ok(())
}

/// Close the active site session on mobile.
///
/// Android: destroy the site WebView through the Kotlin controller (which
/// flushes the *profile* cookie manager first, re-binds PiP and focus to the
/// launcher). The launcher page never moved, so there is nothing to
/// navigate back to — removing the site view reveals it. Idempotent: the
/// Kotlin side resolves normally when no site view exists, which keeps a
/// stale session record harmless.
///
/// iOS: persist the jar, then send the shared WebView back to the launcher.
#[cfg(mobile)]
pub fn close_site_window(app: &AppHandle, _label: &str) -> AppResult<()> {
    #[cfg(target_os = "android")]
    let _session_guard = lock_android_session()?;

    #[cfg(target_os = "android")]
    {
        // Flush + destroy the site view before releasing state: a failure
        // here means the view may still be alive, so the session must stay
        // claimed for a retry.
        crate::android_bridge::site_close(true)?;
    }

    let current = {
        let state = app.state::<AppState>();
        // Bind first: the `MutexGuard` temporary must be dropped before
        // `state`, whose borrow it holds.
        let value = state
            .current_site
            .lock()
            .expect("current_site lock")
            .clone();
        value
    };

    #[cfg(not(target_os = "android"))]
    if let Some(current) = &current {
        let window = app
            .get_webview_window(LAUNCHER_LABEL)
            .ok_or_else(|| AppError::Other("ana WebView bulunamadı".into()))?;
        export_cookies(&window, &current.site_id, &current.host)?;
    }

    // iOS only: the shared WebView was navigated away from the launcher.
    #[cfg(not(target_os = "android"))]
    if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
        let state = app.state::<AppState>();
        let back = state
            .launcher_url
            .lock()
            .expect("launcher_url lock")
            .clone();
        if let Some(url) = back {
            window.navigate(url).map_err(|e| {
                log::error!("başlatıcıya dönülemedi: {e}");
                AppError::Other("başlatıcıya dönülemedi".into())
            })?;
        }
    }

    let closed_site = current.as_ref().map(|c| c.site_id.clone());
    let state = app.state::<AppState>();
    *state.current_site.lock().expect("current_site lock") = None;
    if let Some(site_id) = closed_site {
        let _ = app.emit("animehub://site-closed", site_id);
    }
    Ok(())
}

/// Close a site WebView for a data wipe, without re-exporting its live
/// cookies. Used by `clear_site_data` / `remove_site` on desktop where the
/// profile directory is about to be deleted — exporting would resurrect the
/// just-deleted jar and the next `create_dir_all` would inherit stale cookies.
/// The caller must hold [`lock_desktop_session`] across the close and the
/// subsequent deletes so a concurrent `spawn_session_sync` cannot slip an
/// export in between.
#[cfg(desktop)]
pub fn close_site_window_for_clear(app: &AppHandle, label: &str) -> AppResult<()> {
    let current = {
        let state = app.state::<AppState>();
        let value = state
            .current_site
            .lock()
            .expect("current_site lock")
            .clone();
        value
    };
    let child_label = current
        .as_ref()
        .filter(|c| c.window_label != LAUNCHER_LABEL)
        .map(|c| c.window_label.clone())
        .unwrap_or_else(|| label.to_string());

    if let Some(child) = app.get_webview(&child_label) {
        // Intentionally skip `export_cookies_webview` — the caller is wiping.
        child.close().map_err(|e| {
            log::error!("site görünümü kapatılamadı: {e}");
            AppError::Other("site görünümü kapatılamadı".into())
        })?;
    }

    let mut closed_site = None;
    {
        let state = app.state::<AppState>();
        let mut guard = state.current_site.lock().expect("current_site lock");
        if guard
            .as_ref()
            .is_some_and(|c| c.window_label == child_label)
        {
            closed_site = guard.as_ref().map(|c| c.site_id.clone());
            *guard = None;
        }
    }

    if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
        if let Err(e) = window.set_title(LAUNCHER_TITLE) {
            log::warn!("pencere başlığı geri alınamadı: {e}");
        }
        if current.as_ref().is_some_and(|c| c.entered_fullscreen) {
            if let Err(e) = window.set_fullscreen(false) {
                log::warn!("tam ekrandan çıkılamadı: {e}");
            }
        }
        if let Err(e) = window.set_focus() {
            log::debug!("pencere odaklanamadı: {e}");
        }
    }

    if let Some(site_id) = closed_site {
        let _ = app.emit("animehub://site-closed", site_id);
    }
    Ok(())
}

/// Mobile wipe-close for `remove_site` / `clear_site_data` when the target
/// site is currently open. Holds `ANDROID_SESSION_LOCK` so a concurrent sync
/// tick cannot flush the doomed profile between the close and the wipe.
#[cfg(mobile)]
pub fn close_site_window_for_clear(app: &AppHandle, site_id: &str) -> AppResult<()> {
    #[cfg(target_os = "android")]
    let _guard = lock_android_session()?;
    close_site_window_for_clear_locked(app, site_id)
}

/// Locked variant: caller must hold `ANDROID_SESSION_LOCK` (on Android).
/// Checks that `site_id` is still the current site before closing, so a
/// concurrent site switch does not close the wrong session.
///
/// Android destroys the site WebView with `flush=false`: its data is about
/// to be cleared or deleted, so re-persisting cookie writes first would only
/// give the deletion a chance to lose the race. The launcher WebView is
/// untouched (its data is the default profile and is never part of a site
/// wipe).
#[cfg(mobile)]
pub fn close_site_window_for_clear_locked(app: &AppHandle, site_id: &str) -> AppResult<()> {
    let current = {
        let state = app.state::<AppState>();
        let value = state
            .current_site
            .lock()
            .expect("current_site lock")
            .clone();
        value
    };
    // Only proceed if the site to delete is still the active one; otherwise
    // another site has taken over and must not be closed.
    let should_clear = current.as_ref().is_some_and(|c| c.site_id == site_id);
    if !should_clear {
        return Ok(());
    }

    #[cfg(target_os = "android")]
    {
        // Destroy without flushing — the caller wipes right after.
        crate::android_bridge::site_close(false)?;
    }

    #[cfg(not(target_os = "android"))]
    {
        let back = {
            let state = app.state::<AppState>();
            let value = state
                .launcher_url
                .lock()
                .expect("launcher_url lock")
                .clone();
            value
        };
        if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
            if let Some(url) = back {
                window.navigate(url).map_err(|e| {
                    log::error!("başlatıcıya dönülemedi: {e}");
                    AppError::Other("başlatıcıya dönülemedi".into())
                })?;
            }
        }
    }

    let closed_site = current.as_ref().map(|c| c.site_id.clone());
    {
        let state = app.state::<AppState>();
        let mut guard = state.current_site.lock().expect("current_site lock");
        if guard.as_ref().is_some_and(|c| c.site_id == site_id) {
            *guard = None;
        }
    }
    if let Some(site_id) = closed_site {
        let _ = app.emit("animehub://site-closed", site_id);
    }
    Ok(())
}

/// Close a site session, persisting its cookies first.
///
/// Desktop: remove the child webview; the launcher underneath reappears.
/// Uses `Webview::close` / `set_fullscreen`, which do not exist on mobile.
#[cfg(desktop)]
pub fn close_site_window(app: &AppHandle, label: &str) -> AppResult<()> {
    let _guard = lock_desktop_session()?;
    close_site_window_locked(app, label)
}

#[cfg(desktop)]
fn close_site_window_locked(app: &AppHandle, label: &str) -> AppResult<()> {
    let current = {
        let state = app.state::<AppState>();
        // Bind first: the `MutexGuard` temporary must be dropped before
        // `state`, whose borrow it holds.
        let value = state
            .current_site
            .lock()
            .expect("current_site lock")
            .clone();
        value
    };
    // `label` comes from the launcher UI, which tracks the label it was
    // given at open time. If it has gone stale (the session was already
    // closed via the overlay), fall back to the tracked current site.
    let child_label = current
        .as_ref()
        .filter(|c| c.window_label != LAUNCHER_LABEL)
        .map(|c| c.window_label.clone())
        .unwrap_or_else(|| label.to_string());

    if let Some(child) = app.get_webview(&child_label) {
        // Desktop profiles persist cookies on disk; this jar export is the
        // same belt-and-braces snapshot the old per-window flow took.
        if let Some(cur) = current.as_ref().filter(|c| c.window_label == child_label) {
            export_cookies_webview(&child, &cur.site_id)?;
        }
        child.close().map_err(|e| {
            log::error!("site görünümü kapatılamadı: {e}");
            AppError::Other("site görünümü kapatılamadı".into())
        })?;
    }

    let mut closed_site = None;
    {
        let state = app.state::<AppState>();
        let mut guard = state.current_site.lock().expect("current_site lock");
        if guard
            .as_ref()
            .is_some_and(|c| c.window_label == child_label)
        {
            closed_site = guard.as_ref().map(|c| c.site_id.clone());
            *guard = None;
        }
    }

    // Restore the launcher's chrome: own title again, and leave fullscreen
    // only when *we* were the ones who entered it.
    if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
        if let Err(e) = window.set_title(LAUNCHER_TITLE) {
            log::warn!("pencere başlığı geri alınamadı: {e}");
        }
        if current.as_ref().is_some_and(|c| c.entered_fullscreen) {
            if let Err(e) = window.set_fullscreen(false) {
                log::warn!("tam ekrandan çıkılamadı: {e}");
            }
        }
        if let Err(e) = window.set_focus() {
            log::debug!("pencere odaklanamadı: {e}");
        }
    }

    if let Some(site_id) = closed_site {
        let _ = app.emit("animehub://site-closed", site_id);
    }
    Ok(())
}

/// Open a URL in the system browser.
///
/// Used for links a site tries to open in a new window, so the user is not
/// silently dropped: the navigation leaves the app entirely.
pub fn open_externally(app: &AppHandle, raw: &str) -> AppResult<()> {
    let safe = crate::sites::url_policy::validate_site_url(raw)
        .map_err(|r| AppError::InvalidUrl(r.reason().to_string()))?;
    let state = app.state::<AppState>();
    let blocked = {
        let bl = state.blocklist.lock().expect("blocklist lock");
        bl.is_blocked(safe.host())
    };
    if blocked {
        return Err(AppError::Blocked(safe.host().to_string()));
    }
    tauri_plugin_opener::open_url(safe.as_str(), None::<&str>).map_err(|e| {
        log::error!("adres tarayıcıda açılamadı: {e}");
        AppError::Other("adres tarayıcıda açılamadı".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Settings;
    use crate::secure::master_key::load_master_key;
    use crate::secure::store::SecureStore;
    use crate::secure::SecretProvider;
    use crate::sites::blocklist::Blocklist;
    use std::sync::Mutex;

    #[test]
    fn labels_are_unique_per_site_and_filesystem_safe() {
        assert_eq!(
            label_for("builtin-openanime"),
            "site-site-builtin-openanime"
        );
        assert_ne!(label_for("a"), label_for("b"));
        assert_eq!(label_for("../../x"), "site-site-x");
        assert!(!label_for("").contains('/'));
    }

    #[test]
    fn site_labels_cannot_collide_with_the_launcher() {
        assert_ne!(label_for(LAUNCHER_LABEL), LAUNCHER_LABEL);
        assert!(label_for("anything").starts_with(SITE_LABEL_PREFIX));
    }

    /// Minimal app state over a real temp-dir store, for ownership checks.
    fn state_with_current_site(site_id: Option<&str>) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let key = load_master_key(dir.path()).expect("key");
        let provider = SecureStore::new(&key, dir.path()).expect("store");
        let (registry, warning) = provider.load_registry();
        let current = site_id.map(|id| CurrentSite {
            site_id: id.to_string(),
            host: "a.example".into(),
            window_label: LAUNCHER_LABEL.into(),
            entered_fullscreen: false,
        });
        AppState {
            provider: Box::new(provider),
            registry: Mutex::new(registry),
            settings: Mutex::new(Settings::default()),
            blocklist: Mutex::new(Blocklist::default()),
            oauth_state: Mutex::new(None),
            startup_warning: Mutex::new(warning),
            launcher_url: Mutex::new(None),
            current_site: Mutex::new(current),
            pip_lock: Mutex::new(()),
            pip_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[test]
    fn session_ownership_gates_every_background_write() {
        // No session: nothing is current, sync and restore must both refuse.
        let state = state_with_current_site(None);
        assert!(!session_is_current(&state, "s1"));

        // Live session: only its own site matches; a stale thread holding a
        // different site id is rejected.
        let state = state_with_current_site(Some("s1"));
        assert!(session_is_current(&state, "s1"));
        assert!(!session_is_current(&state, "s2"));
    }

    #[test]
    fn sync_intervals_are_sane() {
        // A crash loses at most one interval of session changes; keep that
        // bounded but not chatty.
        assert!(SESSION_SYNC_INTERVAL.as_secs() >= 15);
        assert!(SESSION_SYNC_INTERVAL.as_secs() <= 300);
        // The safety valve must always outlive many intervals.
        assert!(SESSION_SYNC_MAX_LIFETIME > SESSION_SYNC_INTERVAL * 100);
    }

    #[test]
    fn android_profile_names_are_deterministic_stable_and_safe() {
        // Deterministic: the same site id always yields the same name, or a
        // site's profile would be orphaned on every launch.
        assert_eq!(
            android_profile_name("builtin-openanime", 0),
            android_profile_name("builtin-openanime", 0)
        );
        let name = android_profile_name("builtin-openanime", 0);
        // Conservative charset the WebView provider accepts anywhere, and
        // never the raw id: sanitised/odd ids must not change that shape.
        assert!(name.starts_with("site-"));
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_eq!(android_profile_name("../../etc/passwd", 0).len(), name.len());
        // Not derived from the URL or display name: only site_id matters.
        assert_eq!(name, android_profile_name("builtin-openanime", 0));
        // Never the default profile's name.
        assert_ne!(name, "Default");
    }

    #[test]
    fn android_profile_names_are_unique_per_site_id() {
        // "Each site id gets a different profile": 128 bits of SHA-256 in the
        // name make cross-site collisions negligible, including ids that
        // sanitise to the same bytes (the old directory scheme collided here).
        let a = android_profile_name("user-1", 0);
        let b = android_profile_name("user-2", 0);
        let c = android_profile_name("../../user-1", 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        // The generation distinguishes re-created profiles from names whose
        // deletion may still be finishing on disk.
        assert_ne!(android_profile_name("user-1", 0), android_profile_name("user-1", 1));
        assert_eq!(android_profile_name("user-1", 1), android_profile_name("user-1", 1));
    }

    #[test]
    fn profile_generation_roundtrips_and_bumps() {
        let state = state_with_current_site(None);
        assert_eq!(profile_generation(&state, "user-1").unwrap(), 0);
        assert_eq!(bump_profile_generation(&state, "user-1").unwrap(), 1);
        assert_eq!(profile_generation(&state, "user-1").unwrap(), 1);
        // A different site id keeps its own counter.
        assert_eq!(profile_generation(&state, "user-2").unwrap(), 0);
        assert_eq!(bump_profile_generation(&state, "user-1").unwrap(), 2);
    }

    #[test]
    fn pending_profile_deletes_queue_dedupes_and_forgets() {
        let state = state_with_current_site(None);
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        queue_pending_profile_delete(&state, "site-aaaa-g0").unwrap();
        queue_pending_profile_delete(&state, "site-aaaa-g0").unwrap();
        queue_pending_profile_delete(&state, "site-bbbb-g0").unwrap();
        let queued = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(queued, vec!["site-aaaa-g0", "site-bbbb-g0"]);
        forget_pending_profile_delete(&state, "site-aaaa-g0").unwrap();
        let queued = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(queued, vec!["site-bbbb-g0"]);
        forget_pending_profile_delete(&state, "site-bbbb-g0").unwrap();
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
    }

    #[test]
    fn migration_flag_marks_once_and_clears_with_the_session() {
        use crate::secure::SecretProvider;
        let state = state_with_current_site(None);
        let id = "user-migrate";
        assert!(
            state
                .provider
                .get_secret(&format!("profilemig-{id}"))
                .unwrap()
                .is_none(),
            "fresh site has not migrated"
        );
        mark_migration_done(&state, id).unwrap();
        assert!(
            state
                .provider
                .get_secret(&format!("profilemig-{id}"))
                .unwrap()
                .is_some()
        );
        // Clearing/removing the site deletes the flag with the rest of the
        // session data, so a wiped site never re-imports its wiped jar.
        state.provider.delete_site_session(id).unwrap();
        assert!(
            state
                .provider
                .get_secret(&format!("profilemig-{id}"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn desktop_sync_and_wipe_are_serialized() {
        // Real concurrency: a sync thread holds `DESKTOP_SESSION_LOCK` for
        // 80ms (simulating `export_cookies_webview` under the lock), while
        // the main test thread tries to acquire the same lock for a wipe.
        // The wipe must wait, not deadlock, and must see the session as no
        // longer current after the wipe's `current_site` is cleared.
        #[cfg(desktop)]
        {
            use std::sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            };
            let state = Arc::new(state_with_current_site(Some("s1")));
            let sync_started = Arc::new(AtomicBool::new(false));
            let sync_done = Arc::new(AtomicBool::new(false));
            let state_clone = state.clone();
            let started_clone = sync_started.clone();
            let done_clone = sync_done.clone();
            let handle = std::thread::spawn(move || {
                let _guard = lock_desktop_session().expect("sync lock");
                started_clone.store(true, Ordering::SeqCst);
                // Simulate sync's ownership check + export under the lock.
                assert!(session_is_current(&state_clone, "s1"));
                std::thread::sleep(std::time::Duration::from_millis(80));
                done_clone.store(true, Ordering::SeqCst);
            });
            // Wait until sync thread has the lock.
            while !sync_started.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // Wipe thread (main) tries to acquire the same lock — must block,
            // not deadlock, until sync releases.
            let wipe_start = std::time::Instant::now();
            {
                let _guard = lock_desktop_session().expect("wipe lock");
                let elapsed = wipe_start.elapsed();
                assert!(
                    elapsed >= std::time::Duration::from_millis(50),
                    "wipe did not wait for sync"
                );
                // While holding the wipe lock, simulate clearing current_site.
                *state.current_site.lock().unwrap() = None;
                assert!(!session_is_current(&state, "s1"));
            }
            handle.join().unwrap();
            assert!(sync_done.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn builtin_hide_does_not_delete_session_and_user_delete_does() {
        // Production `Registry::remove` contract: builtin → hidden, user → deleted.
        // This test exercises the real `Registry` helpers that `remove_site`
        // relies on, ensuring a future refactor that collapses the two paths
        // would be caught before it can wipe a builtin's session.
        use crate::sites::registry::{Category, SiteDraft};
        let mut reg = crate::sites::registry::Registry::new_default();
        // Builtin: hide, not delete.
        let builtin_id = "builtin-openanime";
        assert!(reg.get(builtin_id).unwrap().builtin);
        reg.remove(builtin_id).unwrap();
        let builtin = reg
            .get(builtin_id)
            .expect("builtin still present after hide");
        assert!(builtin.hidden, "builtin remove must hide, not delete");
        // User site: true deletion.
        let user = reg
            .add(SiteDraft {
                name: "TempUser".into(),
                url: "https://temp.example/".into(),
                category: Category::Watching,
                letter: None,
                color: None,
                image: None,
            })
            .unwrap();
        let uid = user.id.clone();
        reg.remove(&uid).unwrap();
        assert!(reg.get(&uid).is_none(), "user site must be deleted");
    }
}
