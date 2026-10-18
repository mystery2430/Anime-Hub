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
// `CookieJar` backs the iOS jar swap and the one-time Android profile
// migration; `restore` is used only by the iOS import path (`import_cookies`),
// so it carries exactly that gate — Android's host-compiled migration
// protocol calls it fully qualified, and an ungated `#[cfg(mobile)]` import
// would be dead there. `capture` (qualified at its single call site, so this
// import group keeps the exact shape rustfmt approved before) serves the
// desktop/iOS export path. Android site cookies live in their profile and are
// never snapshotted.
#[cfg(all(mobile, not(target_os = "android")))]
use crate::web::session::restore;
#[cfg(desktop)]
use crate::web::session::{decide_navigation, NavDecision, DNS_REBIND_BLOCK};
use crate::web::session::{profile_dir_name, CookieJar};
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

/// Secret key marking that the site's pre-profile cookie jar has been
/// handed to its WebView profile. Written ONLY after the import fully
/// completed (every cookie accepted and flushed); a failed attempt is never
/// recorded — instead the virgin profile name is retired (queued + generation
/// bump) and the migration retries on a fresh generation. Deleted together
/// with the session data in `delete_site_session`, so a cleared site never
/// re-imports the wiped jar.
const MIGRATION_FLAG_PREFIX: &str = "profilemig-";

/// Secret key holding a JSON list of profile names whose deletion could not
/// run synchronously (loaded in the process that tried, or the provider was
/// temporarily without the API). Drained at the next process start, before
/// any site WebView can open.
pub const PENDING_PROFILE_DELETES_KEY: &str = "android-pending-profile-deletes";

/// Secret key where the RAW bytes of a pending-delete queue are kept before
/// the working copy is salvaged/overwritten — the names inside are the only
/// record of on-disk profile data still awaiting deletion. Written for a
/// corrupt document and for a list with entries dropped, never for a clean
/// queue in the older (names-only) format.
pub const CORRUPT_QUEUE_BACKUP_KEY: &str = "android-pending-profile-deletes-corrupt";

/// Who owns a tombstoned (queued-for-deletion) profile name.
///
/// A profile name is `site-<SHA-256(site_id)[..32]>-g<N>`: the digest is
/// one-way, so the name alone can NEVER be turned back into the site id that
/// derives it. That matters because lifting a tombstone has to be preceded by
/// durable proof that no open can derive the name again — a kill between the
/// tombstone write and the generation write used to leave the stored
/// generation behind, so the drained name came straight back on the next open
/// (P1 / R-07).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingOwner {
    /// Recorded by a path that knew the site: [`run_mobile_delete_protocol`]
    /// and the migration retirement.
    Site(String),
    /// Recorded by the "clear all" orphan sweep: no registered site derived
    /// this name when it was queued.
    Orphan,
    /// An entry written by an older build (names-only list) or salvaged from
    /// corrupt bytes. Never assumed safe: the drain re-matches it against the
    /// registry, and an entry that matches nothing stays queued.
    #[serde(other)]
    #[default]
    Unknown,
}

/// What a queued tombstone still owes.
///
/// The queue is the ONLY durable mechanism the open protocol consults before
/// deriving a profile name ([`resolve_queued_free_generation`]), so an entry
/// that must stay unreachable stays in the queue: dropping it is what would
/// make the name derivable again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingDeleteState {
    /// The provider deletion is not confirmed yet, so the startup drain has to
    /// ask again.
    #[default]
    Owed,
    /// The provider confirmed the deletion (`Deleted`/`Absent`) and NO
    /// registered site derives the name — so there is no site id whose
    /// generation could be secured, a profile name being a one-way digest of
    /// it. The entry becomes a permanent NAME BLOCK instead of being lifted:
    /// it keeps that name out of every future open without asking the provider
    /// again. Should the site id ever come back (a registry restore), the drain
    /// re-matches it, secures the generation durably and only then lifts the
    /// entry.
    Blocked,
}

/// One deferred profile deletion: the name, its owner when one is known, and
/// what it still owes.
///
/// Wire format is a JSON array of these objects. A names-only array (the
/// format older builds wrote) is still read, and an object written before
/// `state` existed reads as [`PendingDeleteState::Owed`] — see
/// [`recover_pending_profile_delete_entries`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingProfileDelete {
    pub name: String,
    #[serde(default)]
    pub owner: PendingOwner,
    #[serde(default)]
    pub state: PendingDeleteState,
}

impl PendingProfileDelete {
    pub fn site(site_id: &str, name: &str) -> Self {
        Self {
            name: name.to_string(),
            owner: PendingOwner::Site(site_id.to_string()),
            state: PendingDeleteState::Owed,
        }
    }

    pub fn orphan(name: &str) -> Self {
        Self {
            name: name.to_string(),
            owner: PendingOwner::Orphan,
            state: PendingDeleteState::Owed,
        }
    }

    pub fn unknown(name: &str) -> Self {
        Self {
            name: name.to_string(),
            owner: PendingOwner::Unknown,
            state: PendingDeleteState::Owed,
        }
    }

    /// A confirmed deletion whose name has no owner: the entry stays in the
    /// queue as the durable block that keeps the name underivable.
    pub fn blocked_orphan(name: &str) -> Self {
        Self {
            name: name.to_string(),
            owner: PendingOwner::Orphan,
            state: PendingDeleteState::Blocked,
        }
    }

    pub fn is_blocked(&self) -> bool {
        self.state == PendingDeleteState::Blocked
    }
}

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

/// Strict shape of a provider-namespace profile name — the same rule Kotlin
/// enforces before it will touch delete/clear:
/// `site-<32 lowercase hex chars>-g<1..10 digits>` with the generation
/// parsing as `u32`. Never loosened: the Default profile and arbitrary
/// provider names must stay unreachable even for a corrupted queue entry.
pub fn is_valid_site_profile_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(ANDROID_PROFILE_PREFIX) else {
        return false;
    };
    let bytes = rest.as_bytes();
    // hex digest (32) + "-g" + digits (1..=10)
    if bytes.len() < 35 || bytes[32] != b'-' || bytes[33] != b'g' {
        return false;
    }
    if !bytes[..32]
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return false;
    }
    let digits = &rest[34..];
    if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    digits.parse::<u32>().is_ok()
}

/// Salvage every valid profile name out of raw (possibly corrupt) bytes.
///
/// The scan is byte-level on purpose: even a structurally broken JSON file
/// keeps readable `site-…-gN` tokens, and dropping only the unparseable
/// part preserves the deletions still owed — silently emptying the queue
/// would strand those names forever (until the orphan sweep at best).
///
/// A candidate is ALWAYS the complete token: the maximal ASCII-digit run
/// after the fixed `site-<32 hex>-g` prefix is taken and only the resulting
/// full name is validated. Trying growing lengths and accepting the first
/// hit (an earlier version of this scan) truncated `…-g123` to `…-g1`,
/// because `…-g1` validates on its own: the wrong profile would be queued
/// for deletion while the real one stayed out of the queue, and
/// [`recover_generation_for`] would derive the next generation from the
/// truncated digits. A digit run that is too long for `u32` (or longer than
/// the 10-digit shape) now fails the whole token instead of being cut back
/// to a shorter, "valid-looking" name.
pub fn scan_valid_profile_names(raw: &[u8]) -> Vec<String> {
    let needle = ANDROID_PROFILE_PREFIX.as_bytes();
    // A valid name is at least 5 + 32 + 2 + 1 bytes: `site-`, the hex
    // digest, `-g` and one generation digit.
    let min_len = 5 + 32 + 2 + 1;
    let mut found = Vec::new();
    let mut i = 0;
    while i + needle.len() <= raw.len() {
        if &raw[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        if i + min_len > raw.len() {
            // Only a truncated tail left — no complete name can fit.
            i += 1;
            continue;
        }
        let mut end = i + min_len;
        while end < raw.len() && raw[end].is_ascii_digit() {
            end += 1;
        }
        match std::str::from_utf8(&raw[i..end]).ok() {
            Some(name) if is_valid_site_profile_name(name) => {
                found.push(name.to_string());
                i = end;
            }
            _ => i += 1,
        }
    }
    found
}

/// Pure recovery rule for the pending-delete queue.
///
/// BOTH wire formats are accepted: the current array of
/// [`PendingProfileDelete`] objects and the older array of plain names, whose
/// entries come back as [`PendingOwner::Unknown`] (an older build could not
/// record an owner, and a name cannot be reversed into one). A document that
/// parses keeps only structurally valid names; a broken document is salvaged
/// name-by-name from the raw bytes.
///
/// Returns `(entries, needs_repair)`. `needs_repair` is true for a corrupt
/// document and for a list with entries dropped — i.e. when the stored bytes
/// no longer say exactly what is owed. A clean names-only list is NOT flagged:
/// backward compatibility means it stays readable as it is, and the next write
/// upgrades it.
pub fn recover_pending_profile_delete_entries(raw: &[u8]) -> (Vec<PendingProfileDelete>, bool) {
    if let Ok(list) = serde_json::from_slice::<Vec<PendingProfileDelete>>(raw) {
        let total = list.len();
        let kept: Vec<PendingProfileDelete> = list
            .into_iter()
            .filter(|entry| is_valid_site_profile_name(&entry.name))
            .collect();
        let dropped = total - kept.len();
        if dropped > 0 {
            log::warn!("bekleyen profil silme listesinden {dropped} geçersiz ad çıkarıldı");
        }
        return (kept, dropped > 0);
    }
    match serde_json::from_slice::<Vec<String>>(raw) {
        Ok(list) => {
            let total = list.len();
            let kept: Vec<PendingProfileDelete> = list
                .into_iter()
                .filter(|name| is_valid_site_profile_name(name))
                .map(|name| PendingProfileDelete::unknown(&name))
                .collect();
            let dropped = total - kept.len();
            if dropped > 0 {
                log::warn!(
                    "eski biçimli bekleyen silme listesinden {dropped} geçersiz ad çıkarıldı"
                );
            }
            (kept, dropped > 0)
        }
        Err(e) => {
            let salvaged = scan_valid_profile_names(raw);
            log::warn!(
                "bekleyen profil silme listesi bozuk ({e}); {} ad ham kopyadan kurtarıldı",
                salvaged.len()
            );
            let entries = salvaged
                .iter()
                .map(|name| PendingProfileDelete::unknown(name))
                .collect();
            (entries, true)
        }
    }
}

/// Name-only view of [`recover_pending_profile_delete_entries`], for the paths
/// that compare names (the open protocol's collision resolution and the
/// generation recovery).
pub fn recover_pending_profile_deletes(raw: &[u8]) -> (Vec<String>, bool) {
    let (entries, needs_repair) = recover_pending_profile_delete_entries(raw);
    (
        entries.into_iter().map(|entry| entry.name).collect(),
        needs_repair,
    )
}

/// Pure recovery rule for a corrupt generation counter: take the highest
/// generation queued for THIS site plus one (0 with an empty queue), so a
/// name whose deletion is still pending is never handed out again.
/// Reusing generation 0 blindly could recreate data Chromium is about to
/// delete; a queued `u32::MAX` has no next value and must refuse.
pub fn recover_generation_for(site_id: &str, queued: &[String]) -> AppResult<u32> {
    let prefix = queued_site_profile_prefix(site_id);
    let max_queued = queued
        .iter()
        .filter_map(|name| {
            name.strip_prefix(&prefix)
                .and_then(|s| s.parse::<u32>().ok())
        })
        .max();
    match max_queued {
        Some(u32::MAX) => Err(AppError::Storage(format!(
            "bekleyen profil silme nesli taştı ({site_id})"
        ))),
        Some(max) => Ok(max + 1),
        None => Ok(0),
    }
}

/// `site-<hash(site_id)>-g` — the fixed prefix shared by all generations of
/// one site's profile names (used for queue matching).
fn queued_site_profile_prefix(site_id: &str) -> String {
    let sample = android_profile_name(site_id, 0);
    sample
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .to_string()
}

/// The persisted generation for `site_id` (0 when never bumped).
///
/// A corrupt value is recovered from the pending-delete queue instead of
/// silently becoming 0 — the old fallback could re-open a name whose
/// on-disk deletion was still queued — and the recovered value is persisted
/// so every later path sees the same truth.
pub fn profile_generation(state: &AppState, site_id: &str) -> AppResult<u32> {
    let key = format!("{PROFILE_GENERATION_PREFIX}{site_id}");
    let Some(bytes) = state.provider.get_secret(&key)? else {
        return Ok(0);
    };
    match std::str::from_utf8(&bytes)
        .ok()
        .map(|s| s.trim().parse::<u32>())
    {
        Some(Ok(generation)) => Ok(generation),
        _ => {
            log::warn!("profil nesli bozuk, kuyruktan geri yükleniyor ({site_id})");
            let queued = read_pending_profile_deletes(state)?;
            let recovered = recover_generation_for(site_id, &queued)?;
            state
                .provider
                .put_secret(&key, recovered.to_string().as_bytes())?;
            Ok(recovered)
        }
    }
}

/// Advance the generation after a delete of the *current* name was accepted
/// or queued, so the next open of this site id derives a fresh name and can
/// never collide with a deletion still finishing on disk.
///
/// `checked_add` — at `u32::MAX` the name space is exhausted. The old
/// `saturating_add` would have pinned the counter and reused the exact name
/// whose deletion was just queued, forever.
pub fn bump_profile_generation(state: &AppState, site_id: &str) -> AppResult<u32> {
    let current = profile_generation(state, site_id)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| AppError::Storage(format!("profil nesli taştı ({site_id})")))?;
    state.provider.put_secret(
        &format!("{PROFILE_GENERATION_PREFIX}{site_id}"),
        next.to_string().as_bytes(),
    )?;
    Ok(next)
}

/// Advance the stored generation past a queued deletion of *this* stored
/// name, if one exists (pure). The bumped value is `max_queued + 1` for this
/// site, which by construction is not in the queue, so exactly one step is
/// needed. `(generation, was_advanced)`.
pub fn resolve_queued_free_generation(
    site_id: &str,
    stored: u32,
    queued: &[String],
) -> AppResult<(u32, bool)> {
    let stored_name = android_profile_name(site_id, stored);
    if !queued.iter().any(|name| name == &stored_name) {
        return Ok((stored, false));
    }
    let prefix = queued_site_profile_prefix(site_id);
    let max_queued = queued
        .iter()
        .filter_map(|name| {
            name.strip_prefix(&prefix)
                .and_then(|s| s.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(stored);
    let next = max_queued
        .checked_add(1)
        .ok_or_else(|| AppError::Storage(format!("bekleyen profil nesli taştı ({site_id})")))?;
    Ok((next, true))
}

/// The generation `site_id` must be stored at for `name` to be underivable
/// (pure, monotone, idempotent).
///
/// `Ok((generation, advanced))`; `advanced == false` means the stored
/// generation is ALREADY past this tombstone, so a repeated drain must not
/// consume another one. Ownership is proved by recomputation
/// (`android_profile_name(site_id, g) == name`) rather than by trusting the
/// digits after the prefix: a tombstone of another site must never move this
/// site's counter. `u32::MAX` has no next value — the error keeps the
/// tombstone durable instead of making the profile reachable again.
pub fn generation_past_name(site_id: &str, stored: u32, name: &str) -> AppResult<(u32, bool)> {
    let prefix = queued_site_profile_prefix(site_id);
    let not_owned = || {
        AppError::Storage(format!(
            "tombstone bu sitenin adını türetmiyor ({site_id}): {name}"
        ))
    };
    let Some(digits) = name.strip_prefix(&prefix) else {
        return Err(not_owned());
    };
    let Ok(generation) = digits.parse::<u32>() else {
        return Err(not_owned());
    };
    if android_profile_name(site_id, generation) != name {
        return Err(not_owned());
    }
    if stored > generation {
        return Ok((stored, false));
    }
    let next = generation.checked_add(1).ok_or_else(|| {
        AppError::Storage(format!(
            "profil nesli taştı, tombstone korunuyor ({site_id})"
        ))
    })?;
    Ok((next, true))
}

/// Durable form of [`generation_past_name`]: read the stored generation,
/// decide, and persist BEFORE the caller lifts the tombstone. A failed write
/// propagates, so the tombstone survives it.
pub fn secure_generation_past_name(
    state: &AppState,
    site_id: &str,
    name: &str,
) -> AppResult<(u32, bool)> {
    let stored = profile_generation(state, site_id)?;
    let (next, advanced) = generation_past_name(site_id, stored, name)?;
    if advanced {
        state.provider.put_secret(
            &format!("{PROFILE_GENERATION_PREFIX}{site_id}"),
            next.to_string().as_bytes(),
        )?;
    }
    Ok((next, advanced))
}

/// Re-match a tombstone whose owner was not recorded against the registered
/// site ids.
///
/// `None` when no registered site derives this name — and also when MORE THAN
/// ONE would, because a digest collision is not something to guess about.
pub fn match_queued_name_to_site(name: &str, site_ids: &[String]) -> Option<String> {
    let mut hits = site_ids.iter().filter(|site_id| {
        let prefix = queued_site_profile_prefix(site_id);
        match name
            .strip_prefix(&prefix)
            .and_then(|digits| digits.parse::<u32>().ok())
        {
            Some(generation) => android_profile_name(site_id, generation) == name,
            None => false,
        }
    });
    let first = hits.next()?.clone();
    if hits.next().is_some() {
        log::warn!("tombstone birden fazla kayıtlı siteyle eşleşti, sahibi belirsiz: {name}");
        return None;
    }
    Some(first)
}

/// Read the deferred profile-deletion queue (names and owners).
///
/// A corrupt list is never treated as empty: valid names are salvaged from
/// the raw bytes, the original bytes are backed up under
/// [`CORRUPT_QUEUE_BACKUP_KEY`], and the working copy is repaired in place.
/// Dropping the queue silently would strand those profile deletions (best
/// case the orphan sweep collects them — worst case a re-open races a
/// deletion nobody recorded any more).
pub fn read_pending_profile_delete_entries(
    state: &AppState,
) -> AppResult<Vec<PendingProfileDelete>> {
    let Some(bytes) = state.provider.get_secret(PENDING_PROFILE_DELETES_KEY)? else {
        return Ok(Vec::new());
    };
    let (entries, needs_repair) = recover_pending_profile_delete_entries(&bytes);
    if needs_repair {
        // Best effort both ways: a failed backup or repair only means the
        // salvage repeats on the next read; never block open behind it.
        if let Err(e) = state.provider.put_secret(CORRUPT_QUEUE_BACKUP_KEY, &bytes) {
            log::warn!("bozuk silme kuyruğu yedeği yazılamadı: {e}");
        }
        match serde_json::to_vec(&entries) {
            Ok(repaired) => {
                if let Err(e) = state
                    .provider
                    .put_secret(PENDING_PROFILE_DELETES_KEY, &repaired)
                {
                    log::warn!("bozuk silme kuyruğu onarılamadı: {e}");
                }
            }
            Err(e) => log::warn!("bozuk silme kuyruğu yeniden yazılamadı: {e}"),
        }
    }
    Ok(entries)
}

/// Name-only view of the queue (the open protocol and the generation recovery
/// compare names, and every existing caller keeps working).
pub fn read_pending_profile_deletes(state: &AppState) -> AppResult<Vec<String>> {
    Ok(read_pending_profile_delete_entries(state)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

/// Queue a tombstone, deduped BY NAME.
///
/// An entry that is already queued keeps its place and nothing is written,
/// except when the call would ADD information:
///
/// * the queued owner is still [`PendingOwner::Unknown`] and the caller knows
///   it, so a queue written by an older build gains what the drain needs to
///   secure a generation;
/// * the queued entry is still [`PendingDeleteState::Owed`] and the caller has
///   a confirmed deletion, so the permanent block is recorded.
///
/// Neither direction is ever reversed: a known owner is not overwritten by
/// another claim, and a permanent block is not downgraded back to "owed" (that
/// would ask the provider to delete a profile it already confirmed gone, and
/// could then lift an entry whose name nothing else keeps unreachable).
fn queue_entry(state: &AppState, entry: PendingProfileDelete) -> AppResult<()> {
    let mut list = read_pending_profile_delete_entries(state)?;
    if let Some(existing) = list.iter_mut().find(|queued| queued.name == entry.name) {
        let mut changed = false;
        if existing.owner == PendingOwner::Unknown && entry.owner != PendingOwner::Unknown {
            existing.owner = entry.owner.clone();
            changed = true;
        }
        if existing.state == PendingDeleteState::Owed && entry.is_blocked() {
            existing.state = PendingDeleteState::Blocked;
            changed = true;
        }
        if !changed {
            return Ok(());
        }
    } else {
        list.push(entry);
    }
    let bytes = serde_json::to_vec(&list)?;
    state
        .provider
        .put_secret(PENDING_PROFILE_DELETES_KEY, &bytes)
}

/// Queue a profile whose site the caller knows — the direct delete protocol
/// and the migration retirement. This is the form the startup drain can secure
/// a generation for without re-matching.
pub fn queue_site_profile_delete(state: &AppState, site_id: &str, name: &str) -> AppResult<()> {
    queue_entry(state, PendingProfileDelete::site(site_id, name))
}

/// Queue a profile with no registered owner (the "clear all" orphan sweep).
pub fn queue_orphan_profile_delete(state: &AppState, name: &str) -> AppResult<()> {
    queue_entry(state, PendingProfileDelete::orphan(name))
}

/// Queue a profile whose owner is NOT known. The drain re-matches such an
/// entry against the registry and keeps it queued when nothing matches, so it
/// is never silently dropped and never assumed safe.
pub fn queue_pending_profile_delete(state: &AppState, name: &str) -> AppResult<()> {
    queue_entry(state, PendingProfileDelete::unknown(name))
}

/// Turn a CONFIRMED, ownerless tombstone into a permanent name block.
///
/// This is the orphan half of "prove the name is underivable before lifting the
/// tombstone": a site-owned tombstone is proved by advancing that site's
/// generation, and an orphan has no site id to advance (the name is a one-way
/// digest of it), so the queue entry itself becomes the durable block. It is
/// written BEFORE the quarantine ledger is retired, and a failed write leaves
/// the entry owed — the deletion is simply retried at the next start.
///
/// Idempotent through [`queue_entry`]: re-blocking an already blocked name
/// writes nothing, and a blocked entry is never downgraded.
pub fn block_pending_profile_delete(state: &AppState, name: &str) -> AppResult<()> {
    queue_entry(state, PendingProfileDelete::blocked_orphan(name))
}

/// Drop a name from the deferred deletion queue. Callers must have made the
/// name underivable durably first — see [`run_pending_delete_drain`] and
/// [`run_mobile_delete_protocol`].
pub fn forget_pending_profile_delete(state: &AppState, name: &str) -> AppResult<()> {
    let list = read_pending_profile_delete_entries(state)?;
    let kept: Vec<PendingProfileDelete> = list
        .into_iter()
        .filter(|entry| entry.name != name)
        .collect();
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

/// The plugin half of the two-phase open protocol.
///
/// Behind a trait so the migrate → durable-flag → open ordering — the part
/// that decides whether a user-visible profile can ever exist while
/// `profilemig-<site>` is missing — is host-testable with a scripted plugin
/// and an injectable [`SecretProvider`]. Production wires it over
/// [`crate::android_bridge`] (see [`BridgeOpenOps`]).
pub trait MobileOpenOps {
    /// Phase 1: import `cookies` into `profile` WITHOUT creating a view.
    fn migrate(&mut self, url: &str, profile: &str, cookies: &[String]) -> AppResult<()>;

    /// Phase 2: build the site WebView bound to `profile` and navigate.
    fn open(&mut self, url: &str, profile: &str, init_script: &str) -> AppResult<()>;
}

/// Production wiring: the two phases over the Android bridge.
#[cfg(target_os = "android")]
pub struct BridgeOpenOps;

#[cfg(target_os = "android")]
impl MobileOpenOps for BridgeOpenOps {
    fn migrate(&mut self, url: &str, profile: &str, cookies: &[String]) -> AppResult<()> {
        crate::android_bridge::site_migrate(url, profile, cookies)
    }

    fn open(&mut self, url: &str, profile: &str, init_script: &str) -> AppResult<()> {
        crate::android_bridge::site_open(url, profile, init_script)
    }
}

/// Resolve the profile for `site_id`, run the pending one-time cookie
/// migration, persist `profilemig-<site_id>` and ONLY THEN open the view.
/// Returns the profile name that was opened.
///
/// WHY THIS ORDERING (the defect this replaces): the previous shape built
/// the view and migrated inside one `site_open` call and wrote the flag
/// afterwards, merely logging a failed write. A missing flag then meant
/// "migration pending" even for a profile the user was already using: the
/// next open hit Kotlin's `ProfileAlreadyExists`, reported it as
/// `migration-partial`, and retired THAT healthy profile — the startup drain
/// deleted its localStorage/IndexedDB and the cookies rolled back to the
/// stale jar. With the flag durable before phase 2, "flag missing" can only
/// ever mean "no view has been shown for this migration yet", so retiring
/// and re-importing is the safe resolution and a shown profile is never
/// queued.
///
/// Contract (proved by the `run_mobile_open_protocol_*` tests):
///  * migration success = every cookie accepted AND the flush durable —
///    Kotlin's strict criterion; anything less arrives as the
///    `migration-partial` sentinel and is retired + retried exactly once on
///    a fresh generation;
///  * the legacy jar is never deleted here — it survives every failure so a
///    retry can re-import it (only clear/remove drops it, with the flag);
///  * a failed flag write ABORTS the open before phase 2: no view exists, so
///    the caller never claims `current_site` and Rust/Kotlin ownership
///    cannot diverge;
///  * a `profile-quarantined` refusal — Kotlin's block on a profile whose
///    last in-place wipe timed out on a provider call it cannot cancel, so a
///    late `removeAllCookies` may still land on a reopened session —
///    propagates in EITHER phase with the same "nothing was opened" guarantee
///    and is deliberately not the `migration-partial` retirement path: the
///    profile is the user's live one, so the fix is to verify the wipe (retry
///    it, or remove the site), never to delete the name and re-import the jar;
///  * every other error (including the MULTI_PROFILE refusal) propagates
///    with the same "nothing was opened" guarantee.
pub fn run_mobile_open_protocol<O: MobileOpenOps>(
    state: &AppState,
    site_id: &str,
    url: &str,
    init_script: &str,
    ops: &mut O,
) -> AppResult<String> {
    // 1. The profile for this site at its current generation — advanced past
    //    a queued deletion of that exact stored name (a stale/corrupt
    //    generation would otherwise hand out a profile whose deletion is
    //    still in flight) and persisted when advanced, so every later path
    //    (clear, delete, orphan sweep) derives the same name.
    let stored_generation = profile_generation(state, site_id)?;
    let queued = read_pending_profile_deletes(state)?;
    let (generation, advanced) =
        resolve_queued_free_generation(site_id, stored_generation, &queued)?;
    if advanced {
        state.provider.put_secret(
            &format!("{PROFILE_GENERATION_PREFIX}{site_id}"),
            generation.to_string().as_bytes(),
        )?;
    }
    let mut profile = android_profile_name(site_id, generation);

    // 2. One-time migration of the pre-profile cookie jar (0.3.4 installs).
    //    localStorage/IndexedDB snapshots are intentionally NOT imported:
    //    the dump was lossy and origin-checking it again would revive the
    //    TOCTOU problems profiles remove. The blobs themselves stay
    //    untouched until the user clears or deletes the site.
    let pending_migration = state
        .provider
        .get_secret(&format!("{MIGRATION_FLAG_PREFIX}{site_id}"))?
        .is_none();
    let mut cookie_headers = Vec::new();
    if pending_migration {
        if let Some(bytes) = state.provider.get_secret(&format!("cookies-{site_id}"))? {
            let jar: CookieJar = serde_json::from_slice(&bytes)?;
            let now = crate::anilist::now_unix();
            for stored in jar.cookies.iter().filter(|c| c.is_live(now)) {
                // Qualified: the `restore` import is iOS-only, and this
                // protocol body is host-compiled (and host-tested).
                if let Some(cookie) = crate::web::session::restore(stored) {
                    cookie_headers.push(cookie.to_string());
                }
            }
        }
    }

    // 3. Phase 1 (migrate, no view) → durable flag → phase 2 (the view).
    //
    //    A `migration-partial` failure means the virgin-profile import did
    //    NOT complete (a cookie was rejected, an exception fired, a callback
    //    or the flush never landed — Kotlin fails instead of faking it).
    //    Retire THAT profile name (queue + generation bump) and retry
    //    exactly once on a fresh generation: a half-imported profile must
    //    never pass as complete, and the legacy snapshot stays untouched
    //    until the import really lands. A second `migration-partial` fails
    //    the open with a retryable, visible error — flag and jar intact, so
    //    the NEXT open starts the whole migration over from a virgin name.
    if pending_migration {
        let mut migrate_result = ops.migrate(url, &profile, &cookie_headers);
        if let Err(AppError::Other(message)) = &migrate_result {
            if !cookie_headers.is_empty()
                && message.contains(animehub_android::MIGRATION_RETRY_SENTINEL)
            {
                log::warn!(
                    "profil göçü tamamlanamadı ({site_id})={message}; profil emekliye ayrılıyor"
                );
                queue_site_profile_delete(state, site_id, &profile)?;
                let (retry_generation, _) = secure_generation_past_name(state, site_id, &profile)?;
                profile = android_profile_name(site_id, retry_generation);
                migrate_result = ops.migrate(url, &profile, &cookie_headers);
                if let Err(AppError::Other(retry_message)) = &migrate_result {
                    if retry_message.contains(animehub_android::MIGRATION_RETRY_SENTINEL) {
                        // Retire this attempt too and surface a retryable error.
                        if let Err(e) = queue_site_profile_delete(state, site_id, &profile) {
                            log::warn!("ikinci göç profili kuyruğa alınamadı: {e}");
                        }
                        if let Err(e) = secure_generation_past_name(state, site_id, &profile) {
                            log::warn!("göç sonrası nesil güvenceye alınamadı: {e}");
                        }
                        return Err(AppError::Other(format!(
                            "site çerezleri profiline taşınamadı: {retry_message}. Çerezler saklandı; sonraki açılışta yeniden denenecek."
                        )));
                    }
                }
            }
        }
        migrate_result?;

        // Durable BEFORE the view exists. A failed write aborts the open on
        // purpose: continuing would show the user a profile whose migration
        // state is unrecorded, and the next open would treat that in-use
        // profile as pending and retire it — the data-loss path this
        // ordering removes. Nothing was built yet, so the abort leaves no
        // orphan view and no ownership to unwind; the jar and the (missing)
        // flag stay exactly as they were and the next open re-imports.
        mark_migration_done(state, site_id).map_err(|e| {
            AppError::Other(format!(
                "site göç durumu kalıcılaştırılamadı ({site_id}): {e}. Site açılmadı; çerezler korundu, sonraki açılışta yeniden denenecek."
            ))
        })?;
    }

    ops.open(url, &profile, init_script)?;
    Ok(profile)
}

/// Android half of one site's clear: wipe the profile in place, then drop the
/// encrypted session blobs — in that order, and never skipping the second half.
///
/// Host-compiled and host-tested: the platform call is injected as
/// `clear_profile` (the same trick [`MobileOpenOps`] uses for the open
/// protocol), so `cargo test --all` proves the ordering on all three host OSes
/// instead of leaving it to the aarch64 compile alone.
///
/// Contract (proved by the `mobile_clear_protocol_*` tests):
///  * the profile is wiped FIRST: if that fails, the error propagates and
///    `delete_site_session` is never reached, so the encrypted jar survives and
///    the user can retry — a kept jar next to a half-wiped profile is
///    recoverable, the reverse is not;
///  * on success `delete_site_session` is NOT optional. The jar and the
///    `profilemig-<site>` flag go together, because a jar kept next to a wiped
///    profile would be re-imported into the next virgin profile and resurrect
///    exactly the cookies the user asked to erase. Kotlin's "the provider
///    deleted nothing because the jar was already empty" success lands here
///    too, so an already-clean profile still drops its blobs — the shape that
///    used to fail the whole clear and strand the jar;
///  * `profilegen-<site>` deliberately survives (see
///    `SecureStore::delete_site_session`): the generation counter must outlive
///    the session so a restored registry can never re-open a profile name whose
///    on-disk deletion is still pending;
///  * the name handed to `clear_profile` is derived from the site id at its
///    CURRENT generation — never from the URL or the display name.
pub fn run_mobile_clear_protocol<C>(
    state: &AppState,
    site_id: &str,
    clear_profile: C,
) -> AppResult<()>
where
    C: FnOnce(&str) -> AppResult<()>,
{
    let generation = profile_generation(state, site_id)?;
    let profile = android_profile_name(site_id, generation);
    clear_profile(&profile)?;
    state.provider.delete_site_session(site_id)?;
    Ok(())
}

/// Android: delete one site's profile, then retire its quarantine ledger —
/// in the ONLY order that keeps a failed cleanup from re-opening an unsafe name.
///
/// THE BUG THIS ORDERING FIXES: Kotlin used to retire the name's
/// [`ProfileQuarantine`] ledger the moment the provider *accepted* the
/// deletion, while the generation bump that makes the name underivable happened
/// afterwards, here. If the bump or its storage write failed, `remove_site`
/// aborted with the registry intact, the next open derived the SAME name, and
/// the gate no longer knew about the `removeAllCookies` deletion that could
/// still be in flight on it — the exact race the per-operation ledger exists to
/// prevent, re-opened by the cleanup path.
///
/// So the sequence is: derive the name from the CURRENT generation → **durably
/// queue the name (tombstone), with the site id that derives it** → delete →
/// durably move the generation past the name → and only for a confirmed
/// deletion, lift the tombstone and retire. The startup drain keeps the very
/// same order for a deletion this process never finished
/// ([`run_pending_delete_drain`]), which is what closes the kill-between-
/// tombstone-and-generation window end to end. Every earlier
/// failure propagates and leaves both the tombstone and the ledger in place,
/// i.e. the name stays blocked; a retry re-runs the whole protocol and is
/// idempotent (the same name is derived again while the bump never landed, and
/// queueing a name that is already queued changes nothing).
///
/// WHY THE TOMBSTONE COMES FIRST (AH-04 / R-07, process death): `deleteProfile`
/// is a provider call that can take arbitrarily long — the reason the watchdogs
/// exist — and "accepted" does not mean Chromium has removed the directory yet.
/// If the process is killed during that call, or between it and the generation
/// write, an in-memory-only ledger is gone with it: nothing durable says the
/// deletion was ever requested, the generation never moved, so the next open
/// derives the SAME name and `getOrCreateProfile` happily reuses the leftover
/// directory — the session the user asked to delete comes back. Queueing the
/// name before touching the provider closes that window with the two mechanisms
/// that already survive a restart: `resolve_queued_free_generation` refuses to
/// hand a queued name to an open, and the startup drain re-runs the deletion
/// until it lands. The cost of a false tombstone (the provider call fails) is
/// one generation number and a deletion that completes at the next start —
/// which is what the caller asked for anyway.
///
/// A `Pending` (deferred) deletion is deliberately NOT retired and keeps its
/// tombstone: the profile still exists on disk and is still loaded in this
/// process, so its ledger stays until the deferred deletion actually lands —
/// the startup drain retires it after durably forgetting the queued name. The
/// bump still happens, so the next open of this site derives a DIFFERENT name
/// and can never collide with a deletion that is still finishing.
///
/// Host-testable on purpose: the platform calls are injected, so `cargo test`
/// proves the order (and the failed-write cases) on every OS instead of leaving
/// it to the aarch64 compile.
pub fn run_mobile_delete_protocol<D, R>(
    state: &AppState,
    site_id: &str,
    delete_profile: D,
    retire_profile: R,
) -> AppResult<crate::android_bridge::SiteProfileAction>
where
    D: FnOnce(&str) -> AppResult<crate::android_bridge::SiteProfileAction>,
    R: FnOnce(&str) -> AppResult<u32>,
{
    use crate::android_bridge::SiteProfileAction;

    let generation = profile_generation(state, site_id)?;
    let profile = android_profile_name(site_id, generation);

    // 1. DURABLE INTENT, before the provider is touched, WITH THE OWNER: a
    //    name cannot be reversed into the site id that derives it, so a
    //    tombstone without an owner cannot secure a generation at the next
    //    start. If this write fails nothing has happened yet: no deletion was
    //    requested, the name is still derivable and still clean, and the
    //    caller gets a retryable error.
    queue_site_profile_delete(state, site_id, &profile)?;

    // 2. The provider call. Its outcome no longer decides whether the name is
    //    remembered — only when the tombstone may be lifted.
    let action = match delete_profile(&profile) {
        Ok(action) => action,
        Err(e) => {
            // Keep the tombstone: the deletion is finished by the startup drain
            // instead of being forgotten with the data still on disk, and the
            // name cannot be handed to an open in the meantime.
            log::warn!("profil silinemedi, bekleyenler kuyruğunda bırakıldı: {profile}");
            return Err(e);
        }
    };

    // 3. Durable: after this write no future open can derive `profile` again
    //    (and the queue already guaranteed it before this line). Monotone and
    //    idempotent — it moves the counter past THIS name, so a repeated run
    //    never burns a second generation, and a failed write propagates with
    //    the tombstone still in place.
    secure_generation_past_name(state, site_id, &profile)?;

    if matches!(action, SiteProfileAction::Pending) {
        // Deferred: the profile still exists and is still loaded in this
        // process. Tombstone and ledger both stay until the drain lands it.
        return Ok(action);
    }

    // 4. Confirmed deleted (or already absent): lift the tombstone, and only
    //    then drop the ledger. A failure here is reported, not swallowed:
    //    keeping both is the conservative direction (the name is already
    //    unreachable), and the retry re-runs the protocol cleanly.
    forget_pending_profile_delete(state, &profile)?;
    let dropped = retire_profile(&profile)?;
    if dropped > 0 {
        log::info!(
            "profil karantinası emekliye ayrıldı ({dropped} bekleyen işlem düştü): {profile}"
        );
    }
    Ok(action)
}

/// What one startup drain round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Tombstones lifted: the deletion was confirmed AND the name is provably
    /// underivable (generation secured, or a name nothing owns).
    pub lifted: usize,
    /// The provider still refuses (the profile is loaded in this process):
    /// kept queued, never reported as deleted.
    pub deferred: usize,
    /// The provider has no profile API on this device: kept queued, and the
    /// remaining names were still tried.
    pub unsupported: usize,
    /// Names held as PERMANENT blocks in this round: already blocked before it,
    /// or blocked by it. A block is not a lifted tombstone — the entry stays in
    /// the queue forever (or until a re-matched owner lets the drain secure a
    /// generation), and the provider is not asked again.
    pub blocked: usize,
    /// Kept because the owner could not be established or the generation could
    /// not be made durable. Such a name is never handed to an open and the
    /// deletion is retried at the next start.
    pub kept_unmatched: Vec<String>,
    /// The bridge was not ready yet: the caller should retry the drain.
    pub bridge_not_ready: bool,
}

/// Secure a confirmed tombstone's generation. `false` — plus a
/// `kept_unmatched` record — when the generation could not be made durable or
/// the name does not belong to this site: the tombstone then stays, which is
/// the conservative direction (the deletion is retried, the name stays
/// blocked).
fn secure_tombstone_generation(
    state: &AppState,
    site_id: &str,
    entry: &PendingProfileDelete,
    report: &mut DrainReport,
) -> bool {
    match secure_generation_past_name(state, site_id, &entry.name) {
        Ok((generation, advanced)) => {
            if advanced {
                log::info!(
                    "ertelenen silme sonrası nesil güvenceye alındı ({site_id} → g{generation})"
                );
            }
            true
        }
        Err(e) => {
            log::warn!(
                "ertelenen silme: nesil güvenceye alınamadı, tombstone korunuyor ({}): {e}",
                entry.name
            );
            report.kept_unmatched.push(entry.name.clone());
            false
        }
    }
}

/// Keep a confirmed tombstone whose name has no owner: never lift it.
///
/// Returns `false` in EVERY case, because no outcome here is allowed to lift
/// the entry — the caller's `if !secured { continue; }` therefore keeps the
/// name queued, which is what stops an open from deriving it again:
///
/// * already [`PendingDeleteState::Blocked`]: counted, nothing written, nothing
///   logged as a warning (this is the steady state, not a hazard);
/// * newly confirmed: the permanent block is written durably. If that write
///   fails the entry stays owed, so the deletion is retried at the next start
///   and the name stays queued either way.
///
/// "No registered site derives this name today" is deliberately NOT treated as
/// proof that none ever will: a registry backup can restore the site id, whose
/// stored generation may sit at or below this tombstone's, and the name is a
/// one-way digest so there is no generation to advance without that id.
fn hold_as_block(state: &AppState, entry: &PendingProfileDelete, report: &mut DrainReport) -> bool {
    if entry.is_blocked() {
        report.blocked += 1;
        return false;
    }
    match block_pending_profile_delete(state, &entry.name) {
        Ok(()) => {
            log::info!(
                "ertelenen profil silmesi kalıcı ad engeline dönüştü (sahibi yok, güvenceye alınacak nesil yok): {}",
                entry.name
            );
            report.blocked += 1;
        }
        Err(e) => {
            log::warn!(
                "ertelenen profil silmesi kalıcı engele çevrilemedi, borçlu olarak korunuyor ({}): {e}",
                entry.name
            );
            report.kept_unmatched.push(entry.name.clone());
        }
    }
    false
}

/// Drain the deferred profile-deletion queue — the startup half of the
/// tombstone protocol, host-testable (platform calls and the owner re-match
/// are injected, so `cargo test` proves the order on every OS).
///
/// THE ORDER, per tombstone: provider delete → **prove durably that no open can
/// derive the name again** → lift the tombstone → retire the quarantine ledger.
///
/// "Prove" means one of exactly two things:
///
///  * the owner is known — recorded when the tombstone was written, or
///    re-matched against the registered site ids for an entry an older build
///    wrote as a bare name — and the site's stored generation is now PAST the
///    tombstone's generation ([`secure_generation_past_name`]: monotone, so a
///    repeated drain does not consume another generation, and durable, so a
///    failed write keeps the tombstone); or
///  * the entry was recorded as an orphan AND still matches no registered site,
///    so nothing derives the name and the confirmed deletion is what makes it
///    unreachable.
///
/// Anything else keeps the tombstone and is reported in
/// [`DrainReport::kept_unmatched`]: a name whose owner cannot be established is
/// never silently dropped and never assumed to be at a safe generation. That is
/// the P1 hole this closes — a kill between the tombstone write and the
/// generation write used to leave the stored generation behind, the drain
/// deleted the profile and lifted the tombstone anyway, and the next open
/// derived the SAME `site-…-gN` name.
pub fn run_pending_delete_drain<D, R, M>(
    state: &AppState,
    entries: &[PendingProfileDelete],
    mut delete_profile: D,
    mut retire_profile: R,
    match_owner: M,
) -> AppResult<DrainReport>
where
    D: FnMut(&str) -> AppResult<crate::android_bridge::SiteProfileAction>,
    R: FnMut(&str) -> AppResult<u32>,
    M: Fn(&str) -> Option<String>,
{
    use crate::android_bridge::SiteProfileAction;
    let mut report = DrainReport::default();

    for entry in entries {
        // A permanent block is not a deletion still owed: the provider already
        // confirmed it, so it is not asked again (no provider call and no
        // warning per start, forever). The entry IS still re-matched against
        // the registry below — an orphan gains an owner when a registry backup
        // restores its site id, and only a durably secured generation may lift
        // it after that.
        let action = if entry.is_blocked() {
            SiteProfileAction::Absent
        } else {
            match delete_profile(&entry.name) {
                Ok(action) => action,
                Err(AppError::Unsupported(_)) => {
                    // No profile API on this device: nothing deletes here until
                    // there is one. Keep it queued so a later provider update can
                    // drain it, and keep going — a feature gap must not abort the
                    // remaining names or be reported as a success.
                    log::warn!(
                        "ertelenen profil silme atlandı: sağlayıcı profilleri desteklemiyor ({})",
                        entry.name
                    );
                    report.unsupported += 1;
                    continue;
                }
                Err(e) => {
                    log::warn!("ertelenen profil silme denenemedi (köprü hazır değil): {e}");
                    report.bridge_not_ready = true;
                    return Ok(report);
                }
            }
        };
        if matches!(action, SiteProfileAction::Pending) {
            // Still in use or still unsupported: keep it queued for a later
            // start. Never reported to the user as deleted.
            log::warn!(
                "ertelenen profil silme bir kez daha ertelendi ({})",
                entry.name
            );
            report.deferred += 1;
            continue;
        }

        // Deleted (or already absent): the bytes are gone. Now make the NAME
        // unreachable durably, BEFORE the record of the deletion is dropped.
        let matched = match_owner(&entry.name);
        let secured = match &entry.owner {
            PendingOwner::Site(site_id) => match &matched {
                Some(other) if other != site_id => {
                    log::warn!(
                        "ertelenen silme: tombstone sahibi çelişiyor (kayıt {site_id}, eşleşen {other}), korunuyor: {}",
                        entry.name
                    );
                    report.kept_unmatched.push(entry.name.clone());
                    false
                }
                _ => secure_tombstone_generation(state, site_id, entry, &mut report),
            },
            PendingOwner::Orphan => match &matched {
                // A site id that was removed and later re-registered derives
                // this name again, so it is secured like an owned tombstone.
                Some(site_id) => secure_tombstone_generation(state, site_id, entry, &mut report),
                // No owner today, and no site id means no generation to secure.
                // That is NOT proof the name stays underivable, so the entry is
                // held as a permanent block instead of lifted.
                None => hold_as_block(state, entry, &mut report),
            },
            PendingOwner::Unknown => match &matched {
                Some(site_id) => secure_tombstone_generation(state, site_id, entry, &mut report),
                None => {
                    log::warn!(
                        "ertelenen silme: tombstone sahibi belirlenemedi, kuyrukta korunuyor ve hiçbir açılışa verilmiyor: {}",
                        entry.name
                    );
                    report.kept_unmatched.push(entry.name.clone());
                    false
                }
            },
        };
        if !secured {
            continue;
        }

        // Durable queue removal FIRST, then the quarantine ledger — the same
        // order `run_mobile_delete_protocol` keeps for the direct path. A
        // retire failure must not undo the fact that the deletion landed and
        // the name is unreachable, so it is logged: keeping the ledger only
        // blocks a name nothing can derive any more.
        forget_pending_profile_delete(state, &entry.name)?;
        report.lifted += 1;
        match retire_profile(&entry.name) {
            Ok(dropped) if dropped > 0 => log::info!(
                "ertelenen profil karantinası emekliye ayrıldı ({dropped} bekleyen kayıt): {}",
                entry.name
            ),
            Ok(_) => {}
            Err(e) => log::warn!(
                "ertelenen profil karantinası emekliye ayrılamadı ({}): {e}",
                entry.name
            ),
        }
        log::info!("ertelenen profil verisi silindi ({})", entry.name);
    }

    Ok(report)
}

/// What one "clear all" orphan sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrphanSweepReport {
    /// Tombstones written BEFORE the provider was asked anything.
    pub queued: usize,
    /// Confirmed deletions turned into permanent name blocks (no owner, so no
    /// generation could be secured). These entries stay in the queue.
    pub blocked: usize,
    /// The provider still has the profile loaded: kept owed, the startup drain
    /// finishes it. Never reported as deleted.
    pub deferred: usize,
    /// No profile API on this device: kept owed, the remaining names were still
    /// tried.
    pub unsupported: usize,
    /// Quarantine ledgers retired — always AFTER the durable block was written.
    pub retired: usize,
    /// A retirement failed. Not a data-safety failure (keeping a ledger is the
    /// conservative direction), so it is counted and logged, not propagated.
    pub retire_failed: usize,
}

/// The "clear all" orphan sweep, in the SAME order as the other two deletion
/// paths ([`run_mobile_delete_protocol`] for a registered site,
/// [`run_pending_delete_drain`] at startup):
///
/// 1. write the durable tombstone;
/// 2. only then ask the provider to delete;
/// 3. on a confirmed deletion, make the name permanently unreachable;
/// 4. only then retire the quarantine ledger.
///
/// The caller (`clear_site_data`, which is `#[cfg(target_os = "android")]` and
/// therefore invisible to `cargo test`) decides WHICH names are orphans: this
/// function is handed the already-filtered list and owns the order, so the
/// sequence is host-tested on every OS instead of only type-checked by the
/// aarch64 compile.
///
/// An orphan has no registered site id and a profile name is a one-way digest
/// of it, so step 3 cannot be a generation bump — it is
/// [`block_pending_profile_delete`], which keeps the entry in the queue as the
/// durable block the open protocol consults. Nothing on a success path drops
/// that entry: the queue is the only mechanism standing between a restored
/// registry (same site id, generation at or below the tombstone's) and the
/// leftover directory of a deletion that `ProfileStore.deleteProfile` may still
/// be finishing asynchronously.
///
/// Failures: a failed tombstone write returns BEFORE any provider call (an
/// unrecorded deletion is the hole this closes); `Unsupported` keeps the entry
/// owed and continues with the remaining names; any other bridge error or
/// timeout keeps the tombstone — the provider may already have started or
/// accepted the work — and is propagated, so the caller surfaces a retryable
/// failure instead of a success.
pub fn run_orphan_profile_sweep<D, R>(
    state: &AppState,
    names: &[String],
    mut delete_profile: D,
    mut retire_profile: R,
) -> AppResult<OrphanSweepReport>
where
    D: FnMut(&str) -> AppResult<crate::android_bridge::SiteProfileAction>,
    R: FnMut(&str) -> AppResult<u32>,
{
    use crate::android_bridge::SiteProfileAction;
    let mut report = OrphanSweepReport::default();

    for owned_name in names {
        let name = owned_name.as_str();

        // (1) Tombstone first. A failed write means the provider is never
        // asked: better an undeleted profile than an unrecorded deletion.
        queue_orphan_profile_delete(state, name)?;
        report.queued += 1;

        // (2) Provider delete — only now.
        let action = match delete_profile(name) {
            Ok(action) => action,
            Err(AppError::Unsupported(_)) => {
                log::warn!(
                    "yetim profil silinemedi: sağlayıcı profilleri desteklemiyor, kuyrukta borçlu kalıyor ({name})"
                );
                report.unsupported += 1;
                continue;
            }
            Err(e) => {
                log::warn!(
                    "yetim profil silme köprüsü cevap vermedi, tombstone korunuyor ({name}): {e}"
                );
                return Err(e);
            }
        };
        if matches!(action, SiteProfileAction::Pending) {
            // Still loaded in this process: the tombstone and its ledger stay,
            // and the startup drain finishes the deletion.
            log::info!("yetim profil hâlâ kullanımda, silme ertelendi ({name})");
            report.deferred += 1;
            continue;
        }

        // (3) Confirmed (`Deleted` or `Absent`): make the name permanently
        // unreachable BEFORE the ledger goes.
        block_pending_profile_delete(state, name)?;
        report.blocked += 1;

        // (4) Quarantine ledger last.
        match retire_profile(name) {
            Ok(dropped) => {
                report.retired += 1;
                log::info!(
                    "yetim site profili temizlendi (düşen karantina kaydı: {dropped}): {name}"
                );
            }
            Err(e) => {
                report.retire_failed += 1;
                log::warn!("yetim profil karantinası emekliye ayrılamadı ({name}): {e}");
            }
        }
    }

    Ok(report)
}

/// Android: open the site in a fresh native WebView bound to its own profile.
///
/// Runs entirely under `ANDROID_SESSION_LOCK`: the outgoing site WebView is
/// closed first (its data lives in its profile — nothing is exported), then
/// [`run_mobile_open_protocol`] resolves the profile name, hands the
/// one-time 0.3.4 cookie jar to the profile cookie manager, persists
/// `profilemig-<site>` and ONLY THEN builds the view — so no WebView the
/// user can see exists while the migration state is not durable. A
/// `MULTI_PROFILE`-unsupported provider aborts with [`AppError::Unsupported`]
/// and no session state is changed.
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

    // 2. Profile resolution, the one-time cookie migration, the durable
    //    migration flag and the view itself — `run_mobile_open_protocol`
    //    owns that ordering (migrate → flag → open), so no view the user can
    //    see exists while `profilemig-<site>` is missing. Any failure,
    //    including the MULTI_PROFILE-unsupported refusal, leaves no session
    //    behind: the view is only built after the flag write succeeded.
    let opened_profile = run_mobile_open_protocol(
        &state,
        &site.id,
        url.as_str(),
        init_script,
        &mut BridgeOpenOps,
    )?;
    log::debug!("site profili açıldı ({}): {opened_profile}", site.id);

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
    use std::sync::{Arc, Mutex};

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
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_eq!(
            android_profile_name("../../etc/passwd", 0).len(),
            name.len()
        );
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
        assert_ne!(
            android_profile_name("user-1", 0),
            android_profile_name("user-1", 1)
        );
        assert_eq!(
            android_profile_name("user-1", 1),
            android_profile_name("user-1", 1)
        );
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
        // Fixtures use the REAL name shape (32 lowercase hex): the reader
        // drops anything `is_valid_site_profile_name` rejects, and the
        // validator is never loosened for tests.
        let name_a = format!("{}{}-g0", ANDROID_PROFILE_PREFIX, "a".repeat(32));
        let name_b = format!("{}{}-g0", ANDROID_PROFILE_PREFIX, "b".repeat(32));
        let state = state_with_current_site(None);
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        queue_pending_profile_delete(&state, &name_a).unwrap();
        queue_pending_profile_delete(&state, &name_a).unwrap();
        queue_pending_profile_delete(&state, &name_b).unwrap();
        let queued = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(queued, vec![name_a.clone(), name_b.clone()]);
        forget_pending_profile_delete(&state, &name_a).unwrap();
        let queued = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(queued, vec![name_b.clone()]);
        forget_pending_profile_delete(&state, &name_b).unwrap();
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
    }

    #[test]
    fn profile_name_validator_is_strict_and_namespace_scoped() {
        let hex32 = "a".repeat(32);
        assert!(is_valid_site_profile_name(&format!("site-{hex32}-g0")));
        assert!(is_valid_site_profile_name(&format!("site-{hex32}-g123456")));
        // Full u32 range stays representable (10 digits).
        assert!(is_valid_site_profile_name(&format!(
            "site-{hex32}-g4294967295"
        )));
        // Never a handle for anything outside our namespace:
        assert!(!is_valid_site_profile_name(&format!(
            "site-{}-g0",
            "a".repeat(31)
        )));
        assert!(!is_valid_site_profile_name(&format!(
            "site-{}-g0",
            "A".repeat(32)
        )));
        assert!(!is_valid_site_profile_name(&format!("site-{hex32}-g")));
        assert!(!is_valid_site_profile_name(&format!("site-{hex32}-g-1")));
        assert!(!is_valid_site_profile_name(&format!("site-{hex32}-g+1")));
        assert!(!is_valid_site_profile_name(&format!(
            "site-{hex32}-g01234567890"
        )));
        // 10 digits beyond u32 must not validate as a "generation".
        assert!(!is_valid_site_profile_name(&format!(
            "site-{hex32}-g9999999999"
        )));
        assert!(!is_valid_site_profile_name("Default"));
        assert!(!is_valid_site_profile_name(&format!("xsite-{hex32}-g0")));
    }

    #[test]
    fn scan_valid_profile_names_salvages_broken_bytes() {
        let name_a = format!("site-{}-g3", "c".repeat(32));
        let name_b = format!("site-{}-g0", "d".repeat(32));
        // Structurally broken JSON: quotes/brackets missing, an invalid
        // short-hex name, and noise around the valid tokens.
        let raw = format!("{{garbage [{name_a}, \"site-e-f0\", tail {name_b} end");
        let found = scan_valid_profile_names(raw.as_bytes());
        assert_eq!(found, vec![name_a, name_b]);
        // Clean input keeps order and nothing extra.
        let clean = serde_json::to_vec(&["unrelated".to_string()]).unwrap();
        assert!(scan_valid_profile_names(&clean).is_empty());
    }

    #[test]
    fn scan_recovers_complete_multi_digit_generations() {
        // The regression this pins: growing-length candidates used to stop at
        // the SHORTEST valid prefix, so `-g123` was salvaged as `-g1` — the
        // wrong profile got queued for deletion while the real one stayed out
        // of the queue, and generation recovery derived from truncated digits.
        let hex = "a".repeat(32);
        for gen in ["g1", "g12", "g123", "g4567", "g4294967295"] {
            let name = format!("site-{hex}-{gen}");
            let raw = format!("[{{broken {name},");
            let found = scan_valid_profile_names(raw.as_bytes());
            assert_eq!(found, vec![name.clone()], "full name must survive: {gen}");
            assert!(is_valid_site_profile_name(&found[0]));
        }
    }

    #[test]
    fn scan_never_truncates_a_generation_into_a_shorter_valid_name() {
        let hex = "b".repeat(32);
        let name = format!("site-{hex}-g123");
        let truncated = format!("site-{hex}-g1");
        let raw = format!("\"{name}\"");
        let found = scan_valid_profile_names(raw.as_bytes());
        assert_eq!(found, vec![name]);
        assert!(
            !found.contains(&truncated),
            "the truncated prefix is a DIFFERENT profile and must not be salvaged"
        );
        // Repeated reads of the same bytes are deterministic (the queue is
        // re-salvaged on every read until it is repaired).
        assert_eq!(found, scan_valid_profile_names(raw.as_bytes()));
    }

    #[test]
    fn scan_refuses_generations_beyond_u32_instead_of_cutting_them_down() {
        let hex = "c".repeat(32);
        // 4294967296 = u32::MAX + 1: right shape, no representable
        // generation. Cutting it back to `…-g429496729` would salvage a
        // name that was never queued.
        let overflow = format!("site-{hex}-g4294967296");
        assert!(scan_valid_profile_names(overflow.as_bytes()).is_empty());
        // Longer than the 10-digit shape the validator allows.
        let too_long = format!("site-{hex}-g12345678901");
        assert!(scan_valid_profile_names(too_long.as_bytes()).is_empty());
        // The largest representable generation is still salvaged in full.
        let max = format!("site-{hex}-g4294967295");
        assert_eq!(scan_valid_profile_names(max.as_bytes()), vec![max]);
    }

    #[test]
    fn scan_handles_lookalikes_and_token_boundaries_exactly() {
        let hex = "d".repeat(32);
        // Shape violations salvage nothing.
        for bad in [
            format!("site-{hex}-g"),                   // no generation digits
            format!("site-{}-g3", "d".repeat(31)),     // short hex digest
            format!("site-{}-g3", "d".repeat(33)),     // long hex digest
            format!("site-{}-G3", hex.to_uppercase()), // uppercase hex + G
            "Default".to_string(),                     // the launcher profile
            "site-not-hex-g3".to_string(),
        ] {
            assert!(
                scan_valid_profile_names(bad.as_bytes()).is_empty(),
                "lookalike must not be salvaged: {bad}"
            );
        }
        // A non-digit ends the token: junk after complete digits does not
        // stop the full name from being salvaged (corrupt neighbours are
        // exactly what this scan exists for).
        let with_junk = format!("site-{hex}-g12xyz");
        assert_eq!(
            scan_valid_profile_names(with_junk.as_bytes()),
            vec![format!("site-{hex}-g12")]
        );
        // An embedded valid token is salvaged as itself — still never
        // anything outside the `site-…` namespace.
        let embedded = format!("xsite-{hex}-g3");
        assert_eq!(
            scan_valid_profile_names(embedded.as_bytes()),
            vec![format!("site-{hex}-g3")]
        );
    }

    #[test]
    fn scan_recovers_every_valid_name_in_broken_json_with_noise() {
        let a = format!("site-{}-g1", "a".repeat(32));
        let b = format!("site-{}-g12", "b".repeat(32));
        let c = format!("site-{}-g123", "c".repeat(32));
        let raw = format!("{{oops [\"{a}\", garbage, {b} , \"site-short-g1\", tail {c}}}");
        let found = scan_valid_profile_names(raw.as_bytes());
        assert_eq!(
            found,
            vec![a, b, c],
            "order preserved; noise and invalid tokens dropped"
        );
    }

    #[test]
    fn salvaged_queue_feeds_generation_recovery_and_collision_resolution() {
        // End-to-end over the REAL production paths: corrupt queue bytes →
        // salvaged names → generation recovery → collision resolution.
        let state = state_with_current_site(None);
        let site = "user-scan";
        let gen7 = android_profile_name(site, 7);
        let gen12 = android_profile_name(site, 12);
        let raw = format!("[{{oops \"{gen7}\", noise {gen12}");
        state
            .provider
            .put_secret(PENDING_PROFILE_DELETES_KEY, raw.as_bytes())
            .unwrap();

        // The reader salvages the COMPLETE names — a truncated `…-g1` would
        // queue a deletion for a profile that was never retired.
        let queued = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(queued, vec![gen7, gen12.clone()]);

        // A corrupt generation counter recovers from those salvaged names.
        state
            .provider
            .put_secret(&format!("{PROFILE_GENERATION_PREFIX}{site}"), b"broken")
            .unwrap();
        assert_eq!(profile_generation(&state, site).unwrap(), 13);

        // Collision resolution never hands out a queued name: stored 12 IS
        // queued, so the derived generation moves past the queue maximum.
        let (generation, advanced) = resolve_queued_free_generation(site, 12, &queued).unwrap();
        assert!(advanced);
        assert_eq!(generation, 13);
        let derived = android_profile_name(site, generation);
        assert!(
            !queued.contains(&derived),
            "derived name must not collide with the salvaged queue"
        );
    }

    #[test]
    fn recover_pending_profile_deletes_reports_corruption() {
        // Parses but contains junk entries: junk dropped, still corrupt.
        let name_a = format!("site-{}-g1", "f".repeat(32));
        let mixed = serde_json::to_vec(&[name_a.clone(), "Default".to_string()]).unwrap();
        let (list, corrupt) = recover_pending_profile_deletes(&mixed);
        assert_eq!(list, vec![name_a.clone()]);
        assert!(corrupt);
        // Broken document: salvaged, always corrupt.
        let broken = format!("[{name_a}").into_bytes();
        let (list, corrupt) = recover_pending_profile_deletes(&broken);
        assert_eq!(list, vec![name_a.clone()]);
        assert!(corrupt);
        // Clean document: untouched, not corrupt.
        let clean = serde_json::to_vec(&[name_a]).unwrap();
        let (list, corrupt) = recover_pending_profile_deletes(&clean);
        assert_eq!(list.len(), 1);
        assert!(!corrupt);
    }

    #[test]
    fn corrupt_queue_is_backed_up_and_repaired_on_read() {
        let state = state_with_current_site(None);
        let name = format!("site-{}-g2", "1".repeat(32));
        // Broken JSON that still contains one valid name.
        let raw = format!("[{{oops {name}").into_bytes();
        state
            .provider
            .put_secret(PENDING_PROFILE_DELETES_KEY, &raw)
            .unwrap();

        let list = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(list, vec![name.clone()], "salvaged, never silently empty");
        // The raw bytes were preserved before the working copy was repaired.
        let backup = state
            .provider
            .get_secret(CORRUPT_QUEUE_BACKUP_KEY)
            .unwrap()
            .expect("corrupt backup must exist");
        assert_eq!(backup.as_slice(), raw.as_slice());
        // The working copy is now valid JSON so later reads stop repairing.
        let reread = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(reread, vec![name]);
    }

    #[test]
    fn corrupt_generation_recovers_from_the_queue_and_persists() {
        let state = state_with_current_site(None);
        let queued_name = android_profile_name("user-1", 3);
        queue_pending_profile_delete(&state, &queued_name).unwrap();
        // A generation for a DIFFERENT site must not influence recovery.
        queue_pending_profile_delete(&state, &android_profile_name("user-2", 7)).unwrap();
        state
            .provider
            .put_secret(
                &format!("{PROFILE_GENERATION_PREFIX}user-1"),
                b"not-a-number",
            )
            .unwrap();

        // Recovered to max-queued-for-this-site + 1 — never silently 0,
        // which could re-open a name whose deletion is still queued.
        assert_eq!(profile_generation(&state, "user-1").unwrap(), 4);
        // And persisted: the next read comes from storage, not recovery.
        let stored = state
            .provider
            .get_secret(&format!("{PROFILE_GENERATION_PREFIX}user-1"))
            .unwrap()
            .expect("recovered generation persisted");
        assert_eq!(String::from_utf8_lossy(&stored), "4");
        // user-1's queue entry no longer collides with the derived name.
        let resolved = android_profile_name("user-1", 4);
        assert!(!read_pending_profile_deletes(&state)
            .unwrap()
            .contains(&resolved));
        // An untouched site still starts at 0.
        assert_eq!(profile_generation(&state, "user-3").unwrap(), 0);
    }

    #[test]
    fn corrupt_generation_with_no_queue_recovers_to_zero() {
        let state = state_with_current_site(None);
        state
            .provider
            .put_secret(&format!("{PROFILE_GENERATION_PREFIX}user-1"), b"3.4")
            .unwrap();
        // No queued deletions at all: 0 is safe (nothing pending for it).
        assert_eq!(profile_generation(&state, "user-1").unwrap(), 0);
    }

    #[test]
    fn bump_profile_generation_refuses_to_overflow() {
        let state = state_with_current_site(None);
        state
            .provider
            .put_secret(
                &format!("{PROFILE_GENERATION_PREFIX}user-1"),
                u32::MAX.to_string().as_bytes(),
            )
            .unwrap();
        // saturating_add would pin at MAX and reuse the queued name forever.
        assert!(bump_profile_generation(&state, "user-1").is_err());
        // The stored value is untouched by the refused bump.
        assert_eq!(profile_generation(&state, "user-1").unwrap(), u32::MAX);
    }

    #[test]
    fn generation_recovery_at_queued_max_refuses() {
        let queued = vec![android_profile_name("user-1", u32::MAX)];
        assert!(recover_generation_for("user-1", &queued).is_err());
        // No queued name → plain 0.
        assert_eq!(recover_generation_for("user-1", &[]).unwrap(), 0);
    }

    #[test]
    fn resolve_queued_free_generation_only_moves_on_real_collision() {
        let own = android_profile_name("user-1", 2);
        let other = android_profile_name("user-2", 2);
        // Nothing queued: unchanged.
        assert_eq!(
            resolve_queued_free_generation("user-1", 2, &[]).unwrap(),
            (2, false)
        );
        // Someone else's name: unchanged.
        assert_eq!(
            resolve_queued_free_generation("user-1", 2, &[other]).unwrap(),
            (2, false)
        );
        // A different generation of this site: unchanged (names differ).
        assert_eq!(
            resolve_queued_free_generation("user-1", 2, &[android_profile_name("user-1", 5)])
                .unwrap(),
            (2, false)
        );
        // The stored name itself is queued: advance past the queue maximum,
        // which by construction cannot be in the queue either.
        assert_eq!(
            resolve_queued_free_generation("user-1", 2, std::slice::from_ref(&own)).unwrap(),
            (3, true)
        );
        assert_eq!(
            resolve_queued_free_generation("user-1", 2, &[own, android_profile_name("user-1", 9)])
                .unwrap(),
            (10, true)
        );
    }

    #[test]
    fn migration_flag_marks_once_and_clears_with_the_session() {
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
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_some());
        // Clearing/removing the site deletes the flag with the rest of the
        // session data, so a wiped site never re-imports its wiped jar.
        state.provider.delete_site_session(id).unwrap();
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_none());
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
            use std::sync::atomic::{AtomicBool, Ordering};
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

    // ---------------------------------------------------------------------
    // Two-phase open protocol (migrate → durable flag → view).
    //
    // These drive the REAL `run_mobile_open_protocol` over the REAL
    // `SecureStore`; only the plugin half is scripted (it is Kotlin on a
    // device) and only the storage WRITE failure is injected — at the real
    // `SecretProvider` boundary, through a wrapper that delegates every
    // other operation to the production store.
    // ---------------------------------------------------------------------

    /// Scripted plugin error, mirroring what `android_bridge` delivers.
    #[derive(Clone)]
    enum ScriptErr {
        Other(String),
        Unsupported,
        /// What `classify_site_open_error` makes of Kotlin's
        /// `profile-quarantined` refusal: the guidance message, typed.
        Quarantined(String),
    }

    impl ScriptErr {
        fn into_app(self) -> AppError {
            match self {
                ScriptErr::Other(message) => AppError::Other(message),
                ScriptErr::Unsupported => {
                    AppError::Unsupported("sağlayıcı çoklu profili desteklemiyor".into())
                }
                ScriptErr::Quarantined(message) => AppError::Quarantined(message),
            }
        }
    }

    /// Records every phase call so ordering — not just outcomes — is asserted.
    struct ScriptedOpenOps {
        calls: Vec<String>,
        cookies_seen: Vec<usize>,
        migrate_results: std::collections::VecDeque<Result<(), ScriptErr>>,
        open_result: Result<(), ScriptErr>,
    }

    impl ScriptedOpenOps {
        fn ok() -> Self {
            Self {
                calls: Vec::new(),
                cookies_seen: Vec::new(),
                migrate_results: std::collections::VecDeque::new(),
                open_result: Ok(()),
            }
        }

        fn with_migrate(results: Vec<Result<(), ScriptErr>>) -> Self {
            Self {
                migrate_results: results.into(),
                ..Self::ok()
            }
        }
    }

    impl MobileOpenOps for ScriptedOpenOps {
        fn migrate(&mut self, _url: &str, profile: &str, cookies: &[String]) -> AppResult<()> {
            self.calls.push(format!("migrate:{profile}"));
            self.cookies_seen.push(cookies.len());
            match self.migrate_results.pop_front() {
                None | Some(Ok(())) => Ok(()),
                Some(Err(e)) => Err(e.into_app()),
            }
        }

        fn open(&mut self, _url: &str, profile: &str, _init_script: &str) -> AppResult<()> {
            self.calls.push(format!("open:{profile}"));
            match &self.open_result {
                Ok(()) => Ok(()),
                Err(e) => Err(e.clone().into_app()),
            }
        }
    }

    /// The real [`SecureStore`] with an injectable `profilemig-*` write
    /// failure — the exact storage-layer fault the protocol must survive
    /// without ever showing a view.
    struct FailingFlagStore {
        inner: SecureStore,
        fail_flag: Arc<std::sync::atomic::AtomicBool>,
        flag_writes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SecretProvider for FailingFlagStore {
        fn backend(&self) -> crate::secure::master_key::Backend {
            self.inner.backend()
        }

        fn load_doc_bytes(&self, name: &str) -> AppResult<Option<zeroize::Zeroizing<Vec<u8>>>> {
            self.inner.load_doc_bytes(name)
        }

        fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
            self.inner.save_doc_bytes(name, bytes)
        }

        fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
            if name.starts_with(MIGRATION_FLAG_PREFIX) {
                self.flag_writes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.fail_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(AppError::Storage("keystore yazılamadı (test)".into()));
                }
            }
            self.inner.put_secret(name, plaintext)
        }

        fn get_secret(&self, name: &str) -> AppResult<Option<zeroize::Zeroizing<Vec<u8>>>> {
            self.inner.get_secret(name)
        }

        fn delete_secret(&self, name: &str) -> AppResult<()> {
            self.inner.delete_secret(name)
        }

        fn wipe_secrets(&self) -> AppResult<usize> {
            self.inner.wipe_secrets()
        }

        fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
            self.inner.delete_site_session(site_id)
        }
    }

    /// State over a real store whose migration-flag writes can be failed and
    /// healed from the test.
    fn failing_flag_state() -> (
        AppState,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let key = load_master_key(dir.path()).expect("key");
        let inner = SecureStore::new(&key, dir.path()).expect("store");
        let (registry, warning) = inner.load_registry();
        let fail_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = FailingFlagStore {
            inner,
            fail_flag: fail_flag.clone(),
            flag_writes: flag_writes.clone(),
        };
        let state = AppState {
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
        };
        (state, fail_flag, flag_writes)
    }

    /// A legacy 0.3.4 jar with two live cookies (one session cookie, one
    /// with a future expiry) — both must reach phase 1 as headers.
    fn live_jar_bytes() -> Vec<u8> {
        let jar = CookieJar {
            site_id: "jar-site".into(),
            saved_at: crate::anilist::now_unix(),
            cookies: vec![
                crate::web::session::StoredCookie {
                    name: "session".into(),
                    value: "abc".into(),
                    domain: Some(".example.com".into()),
                    path: Some("/".into()),
                    expires: None,
                    secure: true,
                    http_only: true,
                    same_site: Some("Lax".into()),
                },
                crate::web::session::StoredCookie {
                    name: "pref".into(),
                    value: "1".into(),
                    domain: None,
                    path: None,
                    expires: Some(crate::anilist::now_unix() + 3600),
                    secure: false,
                    http_only: false,
                    same_site: None,
                },
            ],
        };
        serde_json::to_vec(&jar).unwrap()
    }

    #[test]
    fn protocol_aborts_before_the_view_when_the_flag_write_fails() {
        // Flow: migration succeeds, `profilemig-<site>` cannot be persisted.
        let (state, _fail, flag_writes) = failing_flag_state();
        let id = "user-flagfail";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &live_jar_bytes())
            .unwrap();

        let mut ops = ScriptedOpenOps::ok();
        let err = run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops)
            .expect_err("a failed flag write must abort the open");
        let profile = android_profile_name(id, 0);

        // A visible, retryable error — never the old log-and-continue.
        assert!(matches!(err, AppError::Other(_)), "{err:?}");
        assert!(
            err.to_string().contains("kalıcılaştırılamadı"),
            "error must say the migration state could not be persisted: {err}"
        );
        // The view was NEVER built: phase 1 ran, phase 2 did not. In
        // production that means no Kotlin WebView exists, so Rust claiming
        // nothing keeps both sides' ownership identical.
        assert_eq!(ops.calls, vec![format!("migrate:{profile}")]);
        assert_eq!(ops.cookies_seen, vec![2], "the whole live jar rode phase 1");
        assert_eq!(
            flag_writes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one flag write was attempted"
        );
        // The jar survived for the retry; the flag is still absent; nothing
        // was retired and the generation never moved.
        assert!(state
            .provider
            .get_secret(&format!("cookies-{id}"))
            .unwrap()
            .is_some());
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_none());
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        // The caller only claims the session after Ok — nothing was claimed.
        assert!(state.current_site.lock().unwrap().is_none());
    }

    #[test]
    fn protocol_retires_only_a_never_shown_profile_and_recovers_afterwards() {
        // Flows: (1) migrate ok + flag write fails → no view; (2) reopen with
        // the flag still missing; (3) the profile created in (1) meets the
        // stale jar — Kotlin reports it as already existing; (4) the profile
        // the user was eventually SHOWN is never queued; (6) once the storage
        // fault heals, later opens proceed consistently.
        let (state, fail_flag, _writes) = failing_flag_state();
        let id = "user-reopen";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &live_jar_bytes())
            .unwrap();

        // Attempt 1: migration lands, the flag does not → abort before view.
        let mut ops = ScriptedOpenOps::ok();
        assert!(run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops).is_err());
        let never_shown = android_profile_name(id, 0);
        assert_eq!(ops.calls, vec![format!("migrate:{never_shown}")]);

        // Attempt 2 (storage healed): the profile from attempt 1 exists but
        // was NEVER shown, so Kotlin's `ProfileAlreadyExists` → the retry
        // sentinel → retire + re-import from the intact jar is the safe
        // resolution.
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut ops = ScriptedOpenOps::with_migrate(vec![
            Err(ScriptErr::Other(format!(
                "{}: profil zaten mevcut",
                animehub_android::MIGRATION_RETRY_SENTINEL
            ))),
            Ok(()),
        ]);
        let opened =
            run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops).unwrap();
        let shown = android_profile_name(id, 1);
        assert_eq!(opened, shown);
        assert_eq!(
            ops.calls,
            vec![
                format!("migrate:{never_shown}"),
                format!("migrate:{shown}"),
                format!("open:{shown}"),
            ],
            "retire → retry on a fresh generation → flag → view"
        );
        // Only the never-shown name is queued; the flag became durable in the
        // same attempt that opened the view; the jar was never deleted.
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![never_shown.clone()]
        );
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_some());
        assert!(state
            .provider
            .get_secret(&format!("cookies-{id}"))
            .unwrap()
            .is_some());

        // Attempt 3: the flag is durable → straight to phase 2 on the SAME
        // profile. The profile the user has seen is never re-migrated from
        // the stale jar and never queued for deletion.
        let mut ops = ScriptedOpenOps::ok();
        let opened =
            run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops).unwrap();
        assert_eq!(opened, shown);
        assert_eq!(
            ops.calls,
            vec![format!("open:{shown}")],
            "no migration at all once the flag is durable"
        );
        assert!(ops.cookies_seen.is_empty());
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![never_shown],
            "a shown profile must never be retired"
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 1);
    }

    #[test]
    fn partial_migration_retires_once_retries_and_keeps_the_jar() {
        // Flow 5: a half-finished migration keeps the legacy jar and retries
        // safely on a virgin generation within the same open.
        let (state, fail_flag, _writes) = failing_flag_state();
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let id = "user-partial";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &live_jar_bytes())
            .unwrap();

        let mut ops = ScriptedOpenOps::with_migrate(vec![
            Err(ScriptErr::Other(format!(
                "{}: çerez reddedildi (1/2)",
                animehub_android::MIGRATION_RETRY_SENTINEL
            ))),
            Ok(()),
        ]);
        let opened =
            run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops).unwrap();
        let retry = android_profile_name(id, 1);
        assert_eq!(opened, retry);
        assert_eq!(
            ops.calls,
            vec![
                format!("migrate:{}", android_profile_name(id, 0)),
                format!("migrate:{retry}"),
                format!("open:{retry}"),
            ]
        );
        // The doomed first name is queued, the jar survives, the flag landed.
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![android_profile_name(id, 0)]
        );
        assert!(state
            .provider
            .get_secret(&format!("cookies-{id}"))
            .unwrap()
            .is_some());
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn second_partial_migration_fails_visibly_without_opening_anything() {
        let (state, fail_flag, _writes) = failing_flag_state();
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let id = "user-twice";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &live_jar_bytes())
            .unwrap();

        let partial = || -> Result<(), ScriptErr> {
            Err(ScriptErr::Other(format!(
                "{}: çerez aktarımı zaman aşımına uğradı",
                animehub_android::MIGRATION_RETRY_SENTINEL
            )))
        };
        let mut ops = ScriptedOpenOps::with_migrate(vec![partial(), partial()]);
        let err = run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops)
            .expect_err("two partials must fail the open");
        let message = err.to_string();
        assert!(
            message.contains("taşınamadı") && message.contains("sonraki açılışta"),
            "retryable, user-visible error: {message}"
        );
        // No view, no flag; BOTH doomed names are queued; the jar stays for
        // the next open, which starts the migration over from a virgin name.
        assert_eq!(
            ops.calls,
            vec![
                format!("migrate:{}", android_profile_name(id, 0)),
                format!("migrate:{}", android_profile_name(id, 1)),
            ]
        );
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_none());
        assert!(state
            .provider
            .get_secret(&format!("cookies-{id}"))
            .unwrap()
            .is_some());
        let queued = read_pending_profile_deletes(&state).unwrap();
        assert_eq!(queued.len(), 2);
        assert!(queued.contains(&android_profile_name(id, 0)));
        assert!(queued.contains(&android_profile_name(id, 1)));
    }

    #[test]
    fn unsupported_provider_refuses_without_touching_flag_queue_or_view() {
        // The MULTI_PROFILE refusal must leave every piece of state exactly
        // as it was — no flag, no queue entry, no generation move, no view.
        let (state, fail_flag, _writes) = failing_flag_state();
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let id = "user-unsupported";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &live_jar_bytes())
            .unwrap();

        let mut ops = ScriptedOpenOps::with_migrate(vec![Err(ScriptErr::Unsupported)]);
        let err = run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops)
            .expect_err("unsupported must refuse");
        assert!(matches!(err, AppError::Unsupported(_)), "{err:?}");
        assert_eq!(
            ops.calls,
            vec![format!("migrate:{}", android_profile_name(id, 0))],
            "phase 2 must never run after a refusal"
        );
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_none());
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
    }

    #[test]
    fn quarantine_refusal_never_retires_the_profile_or_writes_the_flag() {
        // Kotlin refuses BOTH open phases for a profile whose last in-place
        // wipe timed out on `removeAllCookies` — a destructive provider call
        // that cannot be cancelled, so it may still delete cookies a reopened
        // session creates (see `ProfileQuarantine` / `SiteOpenGate`). That
        // refusal must be propagated as-is: it is NOT `migration-partial`, so
        // it must not trigger the retire-and-reimport path (which would delete
        // the user's live profile and rebuild it from the stale jar), and it
        // must not persist the flag or claim a session.
        let (state, fail_flag, _writes) = failing_flag_state();
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let id = "user-quarantined";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &live_jar_bytes())
            .unwrap();
        // Scripted exactly as production delivers it: Kotlin's sentinel has
        // already been classified by `android_bridge::classify_site_open_error`
        // (host-tested there) into the typed guidance error.
        let quarantine = || -> ScriptErr {
            ScriptErr::Quarantined(crate::android_bridge::PROFILE_QUARANTINED_MSG.to_string())
        };
        let classified = crate::android_bridge::classify_site_open_error(
            animehub_android::Error::Bridge(format!(
                "{}: çerez temizliği zaman aşımına uğradı, işlem doğrulanamadı",
                animehub_android::QUARANTINE_SENTINEL
            )),
        );
        assert!(
            matches!(classified, AppError::Quarantined(_)),
            "the sentinel must classify to the typed refusal: {classified:?}"
        );
        assert_eq!(
            classified.to_string(),
            crate::android_bridge::PROFILE_QUARANTINED_MSG
        );

        // Phase 1 refusal (pending migration): no retry, no view, no flag.
        let mut ops = ScriptedOpenOps::with_migrate(vec![Err(quarantine())]);
        let err = run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops)
            .expect_err("a quarantined profile must not be migrated into");
        let profile = android_profile_name(id, 0);
        assert!(matches!(err, AppError::Quarantined(_)), "{err:?}");
        assert!(
            !err.to_string()
                .contains(animehub_android::QUARANTINE_SENTINEL),
            "the user reads the guidance, never the raw sentinel: {err}"
        );
        assert!(
            !animehub_android::is_migration_retry(&animehub_android::Error::Bridge(format!(
                "{}: çerez temizliği zaman aşımına uğradı",
                animehub_android::QUARANTINE_SENTINEL
            ))),
            "a quarantine refusal must never be read as a migration retry"
        );
        assert_eq!(
            ops.calls,
            vec![format!("migrate:{profile}")],
            "exactly one attempt: no retirement, no second generation, no view"
        );
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_none());
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        assert!(state
            .provider
            .get_secret(&format!("cookies-{id}"))
            .unwrap()
            .is_some());
        assert!(state.current_site.lock().unwrap().is_none());

        // Phase 2 refusal (flag already durable, the common reopen): the view
        // is not built, the session is not claimed and nothing is retired —
        // the user retries the wipe instead of losing the profile.
        mark_migration_done(&state, id).unwrap();
        let mut ops2 = ScriptedOpenOps {
            open_result: Err(quarantine()),
            ..ScriptedOpenOps::ok()
        };
        let err2 = run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops2)
            .expect_err("a quarantined profile must not be opened");
        assert_eq!(
            ops2.calls,
            vec![format!("open:{profile}")],
            "no migration is re-run once the flag is durable"
        );
        assert!(matches!(err2, AppError::Quarantined(_)), "{err2:?}");
        assert_eq!(
            err2.to_string(),
            crate::android_bridge::PROFILE_QUARANTINED_MSG,
            "the refusal is explained, not forwarded raw"
        );
        assert!(state.current_site.lock().unwrap().is_none());
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        assert!(
            state
                .provider
                .get_secret(&format!("profilemig-{id}"))
                .unwrap()
                .is_some(),
            "the durable flag is untouched by a refused open"
        );
    }

    /// The real [`SecureStore`] with a counted `delete_site_session`, so the
    /// clear protocol can be proved to REACH the session drop on success and to
    /// SKIP it on a failed wipe (not merely to leave the right bytes behind).
    struct CountingSessionStore {
        inner: SecureStore,
        session_deletes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SecretProvider for CountingSessionStore {
        fn backend(&self) -> crate::secure::master_key::Backend {
            self.inner.backend()
        }

        fn load_doc_bytes(&self, name: &str) -> AppResult<Option<zeroize::Zeroizing<Vec<u8>>>> {
            self.inner.load_doc_bytes(name)
        }

        fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
            self.inner.save_doc_bytes(name, bytes)
        }

        fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
            self.inner.put_secret(name, plaintext)
        }

        fn get_secret(&self, name: &str) -> AppResult<Option<zeroize::Zeroizing<Vec<u8>>>> {
            self.inner.get_secret(name)
        }

        fn delete_secret(&self, name: &str) -> AppResult<()> {
            self.inner.delete_secret(name)
        }

        fn wipe_secrets(&self) -> AppResult<usize> {
            self.inner.wipe_secrets()
        }

        fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
            self.session_deletes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.delete_site_session(site_id)
        }
    }

    /// State over a real store whose `delete_site_session` calls are counted.
    fn counting_session_state() -> (AppState, Arc<std::sync::atomic::AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let key = load_master_key(dir.path()).expect("key");
        let inner = SecureStore::new(&key, dir.path()).expect("store");
        let (registry, warning) = inner.load_registry();
        let session_deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = CountingSessionStore {
            inner,
            session_deletes: session_deletes.clone(),
        };
        let state = AppState {
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
        };
        (state, session_deletes)
    }

    fn stored(state: &AppState, name: &str) -> bool {
        state.provider.get_secret(name).unwrap().is_some()
    }

    /// A site whose profile is already clean: Kotlin reports success (its
    /// cookie step saw an empty jar), so the session drop MUST still happen.
    /// This is the shape that used to fail the whole clear — the provider
    /// answered "no cookies deleted" for an empty jar, Rust saw an error and
    /// returned before `delete_site_session`, stranding the encrypted jar and
    /// the migration flag next to a profile that no longer held any cookies.
    #[test]
    fn mobile_clear_protocol_drops_the_session_after_a_successful_wipe() {
        let (state, session_deletes) = counting_session_state();
        let id = "user-clean";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), b"legacy-jar")
            .unwrap();
        mark_migration_done(&state, id).unwrap();
        assert!(stored(&state, &format!("cookies-{id}")));
        assert!(stored(&state, &format!("profilemig-{id}")));

        let mut wiped: Vec<String> = Vec::new();
        run_mobile_clear_protocol(&state, id, |profile| {
            wiped.push(profile.to_string());
            Ok(())
        })
        .expect("a successful in-place wipe is not an error");

        assert_eq!(
            wiped,
            vec![android_profile_name(id, 0)],
            "the wipe gets this site's profile name at its current generation"
        );
        assert_eq!(
            session_deletes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "delete_site_session must be reached exactly once on success"
        );
        assert!(
            !stored(&state, &format!("cookies-{id}")),
            "the encrypted jar goes with the wiped profile"
        );
        assert!(
            !stored(&state, &format!("profilemig-{id}")),
            "and so does the one-time migration flag, so a wiped jar is never re-imported"
        );
    }

    /// The generation counter must SURVIVE the session drop: it is what keeps a
    /// restored registry from re-opening a profile name whose on-disk deletion
    /// may still be pending.
    #[test]
    fn mobile_clear_protocol_keeps_the_generation_counter() {
        let (state, _) = counting_session_state();
        let id = "user-generation";
        assert_eq!(bump_profile_generation(&state, id).unwrap(), 1);
        assert_eq!(bump_profile_generation(&state, id).unwrap(), 2);
        state
            .provider
            .put_secret(&format!("cookies-{id}"), b"jar")
            .unwrap();

        let mut wiped: Vec<String> = Vec::new();
        run_mobile_clear_protocol(&state, id, |profile| {
            wiped.push(profile.to_string());
            Ok(())
        })
        .unwrap();

        assert_eq!(wiped, vec![android_profile_name(id, 2)]);
        assert!(!stored(&state, &format!("cookies-{id}")));
        assert_eq!(
            profile_generation(&state, id).unwrap(),
            2,
            "profilegen outlives the session it belonged to"
        );
    }

    /// A failed wipe must propagate and leave the blobs ALONE: the retry needs
    /// the jar, and a half-wiped profile must never be reported as cleared.
    #[test]
    fn mobile_clear_protocol_skips_the_session_drop_when_the_wipe_fails() {
        let (state, session_deletes) = counting_session_state();
        let id = "user-failed";
        state
            .provider
            .put_secret(&format!("cookies-{id}"), b"legacy-jar")
            .unwrap();
        mark_migration_done(&state, id).unwrap();

        let err = run_mobile_clear_protocol(&state, id, |_| {
            Err(AppError::Other(
                "çerez deposuna erişilemedi: store gone".into(),
            ))
        })
        .expect_err("a failed wipe must not look like a clear");
        assert!(
            err.to_string().contains("çerez deposuna erişilemedi"),
            "{err}"
        );
        assert_eq!(
            session_deletes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "delete_site_session must NOT run after a failed wipe"
        );
        assert!(
            stored(&state, &format!("cookies-{id}")),
            "the jar survives for the retry"
        );
        assert!(
            stored(&state, &format!("profilemig-{id}")),
            "the flag survives too: the profile was not wiped, so nothing may be re-imported over it"
        );
    }

    /// The profile name is derived from the site id only, at the generation the
    /// store knows — never from a URL, a display name or a caller-supplied name.
    #[test]
    fn mobile_clear_protocol_addresses_only_this_sites_profile() {
        let (state, _) = counting_session_state();
        let id = "user-namespace";
        bump_profile_generation(&state, id).unwrap();

        let mut wiped: Vec<String> = Vec::new();
        run_mobile_clear_protocol(&state, id, |profile| {
            wiped.push(profile.to_string());
            Ok(())
        })
        .unwrap();

        let name = &wiped[0];
        assert!(is_valid_site_profile_name(name), "{name}");
        assert!(name.starts_with(ANDROID_PROFILE_PREFIX), "{name}");
        assert_eq!(name, &android_profile_name(id, 1));
        assert_ne!(name, &android_profile_name("another-site", 1));
        assert_ne!(name, "Default");
    }

    /// The real [`SecureStore`] with (a) a log of every durable write — shared
    /// with the injected platform closures, so a test can assert the ORDER of
    /// the delete protocol and not only its outcome — and (b) an injectable
    /// failure count for `profilegen-*` writes, which is the exact fault that
    /// must keep a profile name's quarantine ledger alive.
    struct OrderingStore {
        inner: SecureStore,
        fail_generation_writes: Arc<std::sync::atomic::AtomicUsize>,
        /// Remaining refusals of the tombstone queue itself: what a kill or a
        /// full keystore looks like for the FIRST durable step of a deletion.
        fail_queue_writes: Arc<std::sync::atomic::AtomicUsize>,
        writes: OrderLog,
    }

    impl SecretProvider for OrderingStore {
        fn backend(&self) -> crate::secure::master_key::Backend {
            self.inner.backend()
        }

        fn load_doc_bytes(&self, name: &str) -> AppResult<Option<zeroize::Zeroizing<Vec<u8>>>> {
            self.inner.load_doc_bytes(name)
        }

        fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
            self.inner.save_doc_bytes(name, bytes)
        }

        fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
            // Only DURABLE writes join the ordered log, so a test asserting the
            // order asserts what actually landed. A refused write is recorded
            // distinctly, which keeps the injection visible.
            if name.starts_with(PROFILE_GENERATION_PREFIX)
                && self
                    .fail_generation_writes
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > 0
            {
                self.fail_generation_writes
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                self.writes
                    .lock()
                    .unwrap()
                    .push(format!("put-failed:{name}"));
                return Err(AppError::Storage("keystore yazılamadı (test)".into()));
            }
            if name == PENDING_PROFILE_DELETES_KEY
                && self
                    .fail_queue_writes
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > 0
            {
                self.fail_queue_writes
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                self.writes
                    .lock()
                    .unwrap()
                    .push(format!("put-failed:{name}"));
                return Err(AppError::Storage("keystore yazılamadı (test)".into()));
            }
            self.inner.put_secret(name, plaintext)?;
            self.writes.lock().unwrap().push(format!("put:{name}"));
            Ok(())
        }

        fn get_secret(&self, name: &str) -> AppResult<Option<zeroize::Zeroizing<Vec<u8>>>> {
            self.inner.get_secret(name)
        }

        fn delete_secret(&self, name: &str) -> AppResult<()> {
            self.inner.delete_secret(name)
        }

        fn wipe_secrets(&self) -> AppResult<usize> {
            self.inner.wipe_secrets()
        }

        fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
            self.writes
                .lock()
                .unwrap()
                .push(format!("session:{site_id}"));
            self.inner.delete_site_session(site_id)
        }
    }

    /// The ordered log a delete-protocol test asserts on: every durable write
    /// the store performs, interleaved with the injected platform calls.
    type OrderLog = Arc<Mutex<Vec<String>>>;

    /// `(state, ordered write/call log, remaining generation-write failures)`
    fn ordering_state() -> (AppState, OrderLog, Arc<std::sync::atomic::AtomicUsize>) {
        let (state, writes, fail_generation_writes, _fail_queue_writes) =
            ordering_state_with_queue_failures();
        (state, writes, fail_generation_writes)
    }

    /// `(state, ordered log, remaining generation-write failures, remaining
    /// tombstone-queue write failures)`
    fn ordering_state_with_queue_failures() -> (
        AppState,
        OrderLog,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let key = load_master_key(dir.path()).expect("key");
        let inner = SecureStore::new(&key, dir.path()).expect("store");
        let (registry, warning) = inner.load_registry();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let fail_generation_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fail_queue_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = OrderingStore {
            inner,
            fail_generation_writes: fail_generation_writes.clone(),
            fail_queue_writes: fail_queue_writes.clone(),
            writes: writes.clone(),
        };
        let state = AppState {
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
        };
        (state, writes, fail_generation_writes, fail_queue_writes)
    }

    #[test]
    fn mobile_delete_protocol_retires_only_after_the_generation_is_durable() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, _fail) = ordering_state();
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = "user-delete-order";
        let name = android_profile_name(id, 0);

        let action = run_mobile_delete_protocol(
            &state,
            id,
            |profile| {
                writes.lock().unwrap().push(format!("delete:{profile}"));
                Ok(SiteProfileAction::Deleted)
            },
            |profile| {
                writes.lock().unwrap().push(format!("retire:{profile}"));
                retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(2)
            },
        )
        .unwrap();

        assert_eq!(action, SiteProfileAction::Deleted);
        // THE ORDER IS THE SAFETY PROPERTY: the tombstone is durable before the
        // provider is called, the generation write lands between the deletion
        // and the retirement, the tombstone is lifted only after a confirmed
        // deletion, and all of it is about the same name.
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{name}"),
                format!("put:{PROFILE_GENERATION_PREFIX}{id}"),
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("retire:{name}"),
            ],
            "tombstone → provider → durable bump → lift → retire"
        );
        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(profile_generation(&state, id).unwrap(), 1);
        // A confirmed deletion leaves nothing behind in the queue.
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
    }

    #[test]
    fn mobile_delete_protocol_keeps_the_ledger_when_the_generation_write_fails() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, fail) = ordering_state();
        fail.store(1, std::sync::atomic::Ordering::SeqCst);
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = "user-delete-failed-bump";
        let name = android_profile_name(id, 0);

        let err = run_mobile_delete_protocol(
            &state,
            id,
            |profile| {
                writes.lock().unwrap().push(format!("delete:{profile}"));
                Ok(SiteProfileAction::Deleted)
            },
            |profile| {
                writes.lock().unwrap().push(format!("retire:{profile}"));
                retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap_err();

        // The provider accepted the deletion, but the bump never became durable:
        // the command fails, the ledger is NOT dropped, and the name a retry (or
        // a reopen) derives is still the same one — so it must still be blocked.
        assert!(matches!(err, AppError::Storage(_)), "{err}");
        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{name}"),
                format!("put-failed:{PROFILE_GENERATION_PREFIX}{id}"),
            ],
            "the tombstone is the only durable write, then the refused bump; nothing landed after it"
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        assert_eq!(android_profile_name(id, 0), name);
        // STRONGER THAN BEFORE: the generation write was refused, so the stored
        // generation still derives the deleted name — but the tombstone is
        // durable, and the open path resolves through the queue, so the name is
        // never handed out again even without the bump.
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()]
        );
        let queued = read_pending_profile_deletes(&state).unwrap();
        let (resolved, advanced) =
            resolve_queued_free_generation(id, profile_generation(&state, id).unwrap(), &queued)
                .unwrap();
        assert!(advanced, "a tombstoned name must not be reusable");
        assert_ne!(android_profile_name(id, resolved), name);
    }

    #[test]
    fn mobile_delete_protocol_queues_a_deferred_deletion_and_keeps_its_ledger() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, _fail) = ordering_state();
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = "user-delete-pending";
        let name = android_profile_name(id, 0);

        let action = run_mobile_delete_protocol(
            &state,
            id,
            |profile| {
                writes.lock().unwrap().push(format!("delete:{profile}"));
                Ok(SiteProfileAction::Pending)
            },
            |profile| {
                writes.lock().unwrap().push(format!("retire:{profile}"));
                retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap();

        assert_eq!(action, SiteProfileAction::Pending);
        // The queue write is durable BEFORE the bump, and the ledger survives:
        // a pending profile still exists on disk, so only the startup drain may
        // retire it (after it durably forgets the queued name).
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()]
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{name}"),
                format!("put:{PROFILE_GENERATION_PREFIX}{id}"),
            ],
            "the tombstone is written once (before the provider) and NOT lifted for a deferred deletion"
        );
        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The new generation is a DIFFERENT name, so the next open can never
        // collide with the deletion still finishing on disk.
        assert_eq!(profile_generation(&state, id).unwrap(), 1);
        assert_ne!(android_profile_name(id, 1), name);
    }

    #[test]
    fn mobile_delete_protocol_is_safe_to_retry_after_a_failed_bump() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, fail) = ordering_state();
        fail.store(1, std::sync::atomic::Ordering::SeqCst);
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = "user-delete-retry";
        let name = android_profile_name(id, 0);

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delete = |profile: &str| {
            writes.lock().unwrap().push(format!("delete:{profile}"));
            // First attempt: the provider accepts. Every later attempt finds the
            // profile already gone — the generation cannot distinguish them,
            // because the failed bump is exactly what never moved it.
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if attempt == 0 {
                SiteProfileAction::Deleted
            } else {
                SiteProfileAction::Absent
            })
        };
        assert!(run_mobile_delete_protocol(&state, id, delete, |profile| {
            writes.lock().unwrap().push(format!("retire:{profile}"));
            retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(1)
        })
        .is_err());
        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Retry with the storage fault healed: the SAME name is derived again
        // (the bump never landed), the protocol completes, and the ledger is
        // dropped exactly once — for that name.
        let action = run_mobile_delete_protocol(&state, id, delete, |profile| {
            writes.lock().unwrap().push(format!("retire:{profile}"));
            retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(1)
        })
        .unwrap();
        assert_eq!(action, SiteProfileAction::Absent);
        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{name}"),
                format!("put-failed:{PROFILE_GENERATION_PREFIX}{id}"),
                // The retry re-queues the SAME name, which the deduped queue
                // already holds, so it writes nothing.
                format!("delete:{name}"),
                format!("put:{PROFILE_GENERATION_PREFIX}{id}"),
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("retire:{name}"),
            ],
            "the retry re-derives the SAME name, and only its durable bump is followed by a lifted tombstone and a retirement"
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 1);
    }

    #[test]
    fn mobile_delete_protocol_retires_each_generation_exactly_once_on_repeat() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, _fail) = ordering_state();
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = "user-delete-repeat";
        let first = android_profile_name(id, 0);
        let second = android_profile_name(id, 1);

        for action in [SiteProfileAction::Deleted, SiteProfileAction::Absent] {
            let outcome = run_mobile_delete_protocol(
                &state,
                id,
                |profile| {
                    writes.lock().unwrap().push(format!("delete:{profile}"));
                    Ok(action)
                },
                |profile| {
                    writes.lock().unwrap().push(format!("retire:{profile}"));
                    retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(0)
                },
            )
            .unwrap();
            assert_eq!(outcome, action);
        }

        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{first}"),
                format!("put:{PROFILE_GENERATION_PREFIX}{id}"),
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("retire:{first}"),
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{second}"),
                format!("put:{PROFILE_GENERATION_PREFIX}{id}"),
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("retire:{second}"),
            ],
            "each generation: tombstone → provider → bump → lift → retire"
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 2);
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
    }

    #[test]
    fn mobile_delete_protocol_tombstones_the_name_before_the_provider_is_called() {
        // AH-04 / R-07. `deleteProfile` is a provider call that can take
        // arbitrarily long — the reason the watchdogs exist — and "accepted"
        // does not mean Chromium removed the directory. A process kill inside
        // that call used to leave NO durable trace: the Kotlin ledger is
        // in-memory, the generation had not moved, so the next open derived the
        // same name and `getOrCreateProfile` reused the leftover directory —
        // the session the user asked to delete came back. The tombstone is what
        // survives the kill, so it has to exist before the call, not after it.
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, _fail) = ordering_state();
        let id = "user-delete-tombstone";
        let name = android_profile_name(id, 0);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let queued_during_call = seen.clone();

        let action = run_mobile_delete_protocol(
            &state,
            id,
            |profile| {
                // Read the durable state AT THE MOMENT the provider is asked:
                // this is exactly what a kill inside the call would leave.
                *queued_during_call.lock().unwrap() = read_pending_profile_deletes(&state).unwrap();
                writes.lock().unwrap().push(format!("delete:{profile}"));
                Ok(SiteProfileAction::Deleted)
            },
            |profile| {
                writes.lock().unwrap().push(format!("retire:{profile}"));
                Ok(0)
            },
        )
        .unwrap();

        assert_eq!(action, SiteProfileAction::Deleted);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![name.clone()],
            "the name must already be durably queued while the provider call is in flight"
        );
        let log = writes.lock().unwrap().clone();
        assert_eq!(
            log[0],
            format!("put:{PENDING_PROFILE_DELETES_KEY}"),
            "the tombstone is the first durable write of the protocol"
        );
        assert_eq!(log[1], format!("delete:{name}"));
    }

    #[test]
    fn a_failed_provider_delete_keeps_the_tombstone_so_the_name_cannot_come_back() {
        // The kill scenario, expressed as the closest thing a host test can do:
        // the deletion was requested, the provider never answered successfully,
        // and nothing in memory survives. What a restart can see is the durable
        // queue and the generation counter, so those are what is asserted.
        let (state, writes, _fail) = ordering_state();
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = "user-delete-provider-failed";
        let name = android_profile_name(id, 0);

        let err = run_mobile_delete_protocol(
            &state,
            id,
            |_profile| Err(AppError::Other("sağlayıcı yanıt vermedi".into())),
            |profile| {
                writes.lock().unwrap().push(format!("retire:{profile}"));
                retired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap_err();

        assert!(matches!(err, AppError::Other(_)), "{err}");
        assert_eq!(
            retired.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the ledger must stay: the deletion is not confirmed"
        );
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()],
            "the tombstone survives, so the startup drain finishes the deletion instead of forgetting it"
        );
        // No bump — the deletion is unconfirmed — and none is needed, because
        // the open path resolves the generation through the queue.
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        let queued = read_pending_profile_deletes(&state).unwrap();
        let (resolved, advanced) = resolve_queued_free_generation(id, 0, &queued).unwrap();
        assert!(
            advanced,
            "a tombstoned name must never be handed to an open"
        );
        assert_ne!(
            android_profile_name(id, resolved),
            name,
            "the reopened session gets a different profile, never the leftover directory"
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![format!("put:{PENDING_PROFILE_DELETES_KEY}")],
            "only the tombstone landed"
        );
    }

    // -----------------------------------------------------------------------
    // The startup drain: a tombstone is lifted ONLY once the name it records is
    // provably underivable. Each test starts from the durable state a restart
    // would actually see after a kill at one point of the delete protocol —
    // nothing in memory survives a kill, so the queue and the generation
    // counter are the whole truth.
    //
    // What a host test CANNOT prove: when the real Android provider finishes
    // removing the directory from disk. `Deleted` here means "the provider
    // confirmed", which is the same assumption the direct path makes; the
    // device-side behaviour stays an open item in
    // docs/android-storage-isolation.md.
    // -----------------------------------------------------------------------

    /// Write the queue in the CURRENT format (objects with an owner).
    fn write_queue(state: &AppState, entries: Vec<PendingProfileDelete>) {
        let bytes = serde_json::to_vec(&entries).unwrap();
        state
            .provider
            .put_secret(PENDING_PROFILE_DELETES_KEY, &bytes)
            .unwrap();
    }

    /// Write the queue in the LEGACY format an older build produced: a plain
    /// JSON array of names, with no owner recorded anywhere.
    fn write_legacy_queue(state: &AppState, names: Vec<String>) {
        let bytes = serde_json::to_vec(&names).unwrap();
        state
            .provider
            .put_secret(PENDING_PROFILE_DELETES_KEY, &bytes)
            .unwrap();
    }

    /// Owner re-match that finds nothing — the shape a drain sees when no
    /// registered site derives the tombstoned name.
    fn no_owner(_name: &str) -> Option<String> {
        None
    }

    fn write_generation(state: &AppState, site_id: &str, generation: u32) {
        state
            .provider
            .put_secret(
                &format!("{PROFILE_GENERATION_PREFIX}{site_id}"),
                generation.to_string().as_bytes(),
            )
            .unwrap();
    }

    /// Scenario 1: the tombstone is durable, the provider was never called.
    #[test]
    fn drain_after_a_kill_before_the_provider_ran_finishes_the_deletion() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-drain-before-provider";
        let name = android_profile_name(id, 0);
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);
        assert_eq!(profile_generation(&state, id).unwrap(), 0);

        let deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (d, r) = (deletes.clone(), retires.clone());
        let expected = name.clone();
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            move |profile| {
                assert_eq!(
                    profile,
                    expected.as_str(),
                    "the queued name is the one deleted"
                );
                d.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SiteProfileAction::Deleted)
            },
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 1, "{report:?}");
        assert!(report.kept_unmatched.is_empty(), "{report:?}");
        assert_eq!(deletes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(retires.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The generation moved past the deleted name BEFORE the tombstone was
        // lifted, so the next open cannot derive it.
        assert_eq!(profile_generation(&state, id).unwrap(), 1);
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        assert_ne!(
            android_profile_name(id, profile_generation(&state, id).unwrap()),
            name
        );
    }

    /// Scenario 2 — THE P1 BUG: the provider accepted the deletion and the
    /// process died before the generation write landed. The old drain deleted
    /// the profile, lifted the tombstone and left the stored generation at N,
    /// so the next open derived the very same `site-…-gN` name.
    #[test]
    fn drain_after_a_kill_between_the_delete_and_the_generation_write_never_reopens_the_name() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-drain-before-bump";
        let name = android_profile_name(id, 0);
        // Durable state after the kill: tombstone present, generation never
        // bumped, profile already gone from the provider's point of view.
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);
        write_generation(&state, id, 0);

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let expected = name.clone();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            move |profile| {
                assert_eq!(profile, expected.as_str());
                Ok(SiteProfileAction::Absent)
            },
            |_profile| Ok(0),
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 1, "{report:?}");
        // The stored generation is now PAST the drained name, and that write is
        // durable before the tombstone was lifted.
        assert_eq!(
            profile_generation(&state, id).unwrap(),
            1,
            "the drain must secure the generation the killed process never wrote"
        );
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        let next_open = android_profile_name(id, profile_generation(&state, id).unwrap());
        assert_ne!(
            next_open, name,
            "the deleted profile name must never come back"
        );
    }

    /// Scenario 3: the generation write landed, the process died before the
    /// tombstone was lifted. The drain must NOT consume a second generation.
    #[test]
    fn drain_after_a_kill_between_the_generation_write_and_the_lift_does_not_bump_again() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-drain-after-bump";
        let name = android_profile_name(id, 0);
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);
        write_generation(&state, id, 1);

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Absent),
            |_profile| Ok(0),
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 1, "{report:?}");
        assert_eq!(
            profile_generation(&state, id).unwrap(),
            1,
            "already past the tombstone: idempotent, no extra generation burned"
        );
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
    }

    /// Scenario 4: retirement fails AFTER the tombstone was lifted. The
    /// deletion landed and the name is unreachable, so the drain does not undo
    /// either; the surviving ledger only blocks a name nothing can derive.
    #[test]
    fn drain_lifts_the_tombstone_when_retirement_fails_afterwards() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-drain-retire-failed";
        let name = android_profile_name(id, 0);
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            |_profile| Err(AppError::Other("karantina defteri düşürülemedi".into())),
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 1, "{report:?}");
        assert!(!report.bridge_not_ready);
        assert_eq!(profile_generation(&state, id).unwrap(), 1);
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
    }

    /// Scenario 5: whichever point the process died at, the name the NEXT open
    /// derives is different from the drained one, and the queue is empty.
    #[test]
    fn every_kill_point_leaves_the_next_open_with_a_different_name() {
        use crate::android_bridge::SiteProfileAction;

        // (tombstoned generation, stored generation after the kill, provider
        //  answer at the next start)
        let kill_points = [
            ("before the provider ran", 0u32, 0u32, false),
            ("after the delete, before the generation write", 0, 0, true),
            ("after the generation write, before the lift", 0, 1, true),
        ];
        for (label, tombstoned, stored, already_gone) in kill_points {
            let state = state_with_current_site(None);
            let id = "user-drain-matrix";
            let name = android_profile_name(id, tombstoned);
            write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);
            write_generation(&state, id, stored);

            let entries = read_pending_profile_delete_entries(&state).unwrap();
            let report = run_pending_delete_drain(
                &state,
                &entries,
                |_profile| {
                    Ok(if already_gone {
                        SiteProfileAction::Absent
                    } else {
                        SiteProfileAction::Deleted
                    })
                },
                |_profile| Ok(0),
                no_owner,
            )
            .unwrap();

            assert_eq!(report.lifted, 1, "{label}: {report:?}");
            assert!(
                read_pending_profile_deletes(&state).unwrap().is_empty(),
                "{label}: the tombstone must be lifted once the name is unreachable"
            );
            let next = android_profile_name(id, profile_generation(&state, id).unwrap());
            assert_ne!(next, name, "{label}: the drained name came back");
            // And the open protocol's own collision resolution agrees.
            let queued = read_pending_profile_deletes(&state).unwrap();
            let (resolved, advanced) = resolve_queued_free_generation(
                id,
                profile_generation(&state, id).unwrap(),
                &queued,
            )
            .unwrap();
            assert!(!advanced, "{label}: nothing is queued any more");
            assert_ne!(android_profile_name(id, resolved), name, "{label}");
        }
    }

    /// Scenario 6: the generation write is refused. The tombstone MUST survive
    /// it — lifting without the durable generation is exactly the P1 hole.
    #[test]
    fn a_failed_generation_write_keeps_the_tombstone_queued() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, fail) = ordering_state();
        fail.store(1, std::sync::atomic::Ordering::SeqCst);
        let id = "user-drain-failed-generation";
        let name = android_profile_name(id, 0);
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);

        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 0, "{report:?}");
        assert_eq!(report.kept_unmatched, vec![name.clone()], "{report:?}");
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()],
            "the tombstone survives a refused generation write"
        );
        assert_eq!(
            retires.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the ledger is never dropped for a name that is still derivable"
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        assert!(
            writes
                .lock()
                .unwrap()
                .contains(&format!("put-failed:{PROFILE_GENERATION_PREFIX}{id}")),
            "the refused write must be visible in the ordered log"
        );
        // The name is still blocked for an open, through the queue.
        let queued = read_pending_profile_deletes(&state).unwrap();
        let (resolved, advanced) = resolve_queued_free_generation(id, 0, &queued).unwrap();
        assert!(advanced);
        assert_ne!(android_profile_name(id, resolved), name);
    }

    /// Scenario 7: draining twice (deferred first, confirmed later, then a
    /// stale re-read of the same entries) consumes exactly ONE generation.
    #[test]
    fn repeated_drains_do_not_consume_another_generation() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-drain-repeat";
        let name = android_profile_name(id, 0);
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        // The drain takes its provider closure BY VALUE, so each round gets its
        // own closure over the same attempt counter: round 1 is still deferred,
        // every later round confirms.
        let scripted = |counter: Arc<std::sync::atomic::AtomicUsize>| {
            move |_profile: &str| {
                let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(if attempt == 0 {
                    SiteProfileAction::Pending
                } else {
                    SiteProfileAction::Deleted
                })
            }
        };

        // Round 1: still loaded in this process → deferred, tombstone kept.
        let first = run_pending_delete_drain(
            &state,
            &entries,
            scripted(attempts.clone()),
            |_p| Ok(0),
            no_owner,
        )
        .unwrap();
        assert_eq!(first.deferred, 1, "{first:?}");
        assert_eq!(first.lifted, 0);
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()]
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 0);

        // Round 2 (next start): confirmed → generation secured once, lifted.
        let entries2 = read_pending_profile_delete_entries(&state).unwrap();
        let second = run_pending_delete_drain(
            &state,
            &entries2,
            scripted(attempts.clone()),
            |_p| Ok(0),
            no_owner,
        )
        .unwrap();
        assert_eq!(second.lifted, 1, "{second:?}");
        assert_eq!(profile_generation(&state, id).unwrap(), 1);

        // Round 3: a stale copy of the same entries (a restart that read the
        // queue before the lift landed). The generation must NOT move again.
        let third = run_pending_delete_drain(
            &state,
            &entries2,
            scripted(attempts.clone()),
            |_p| Ok(0),
            no_owner,
        )
        .unwrap();
        assert_eq!(third.lifted, 1, "{third:?}");
        assert_eq!(
            profile_generation(&state, id).unwrap(),
            1,
            "a repeated drain is idempotent: one generation, not two"
        );
        assert_ne!(android_profile_name(id, 1), name);
    }

    /// Backward compatibility for the OBJECT format as well: a queue written
    /// after owners were recorded but before `state` existed reads as OWED, so
    /// an unfinished deletion is never mistaken for a confirmed one (which
    /// would skip the provider call and strand the profile).
    #[test]
    fn a_queue_object_written_before_the_block_state_reads_as_owed() {
        let state = state_with_current_site(None);
        let name = android_profile_name("user-pre-state", 1);
        let raw = format!("[{{\"name\":\"{name}\",\"owner\":\"orphan\"}}]");
        state
            .provider
            .put_secret(PENDING_PROFILE_DELETES_KEY, raw.as_bytes())
            .unwrap();

        let (entries, needs_repair) = recover_pending_profile_delete_entries(raw.as_bytes());
        assert!(
            !needs_repair,
            "a pre-state object list is valid JSON, not corrupt bytes"
        );
        assert_eq!(
            entries,
            vec![PendingProfileDelete::orphan(&name)],
            "a missing state field reads as OWED"
        );
        assert!(!entries[0].is_blocked());

        // Blocking writes the field, and the entry stays a plain object an
        // older build can still read (name + owner are unchanged).
        block_pending_profile_delete(&state, &name).unwrap();
        let bytes = state
            .provider
            .get_secret(PENDING_PROFILE_DELETES_KEY)
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains(&format!("\"name\":\"{name}\"")), "{text}");
        assert!(text.contains("\"owner\":\"orphan\""), "{text}");
        assert!(text.contains("\"state\":\"blocked\""), "{text}");
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::blocked_orphan(&name)]
        );
    }

    /// Backward compatibility: a queue written by an older build (names only)
    /// is read, its owner is re-matched against the registry, and the drained
    /// tombstone still secures the generation.
    #[test]
    fn a_legacy_names_only_queue_is_read_and_matched_against_the_registry() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-legacy-queue";
        let name = android_profile_name(id, 2);
        write_legacy_queue(&state, vec![name.clone()]);

        // The legacy bytes stay readable and are NOT flagged as corrupt (only
        // a broken document or dropped entries are).
        let raw = state
            .provider
            .get_secret(PENDING_PROFILE_DELETES_KEY)
            .unwrap()
            .unwrap();
        let (entries, needs_repair) = recover_pending_profile_delete_entries(&raw);
        assert!(!needs_repair, "a clean legacy list must not be rewritten");
        assert_eq!(entries, vec![PendingProfileDelete::unknown(&name)]);

        // The owner comes back from the registered site ids.
        let ids = vec![id.to_string(), "some-other-site".to_string()];
        assert_eq!(match_queued_name_to_site(&name, &ids).as_deref(), Some(id));

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            |_profile| Ok(1),
            |profile| match_queued_name_to_site(profile, &ids),
        )
        .unwrap();

        assert_eq!(report.lifted, 1, "{report:?}");
        assert_eq!(profile_generation(&state, id).unwrap(), 3);
        assert!(read_pending_profile_deletes(&state).unwrap().is_empty());
        assert_ne!(android_profile_name(id, 3), name);
    }

    /// An ownerless tombstone that matches no registered site is NEVER dropped
    /// silently: it stays queued, is reported, and the ledger survives.
    #[test]
    fn an_unmatched_tombstone_is_kept_and_reported_not_silently_dropped() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-unmatched";
        let name = android_profile_name(id, 1);
        write_legacy_queue(&state, vec![name.clone()]);

        let deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let d = deletes.clone();
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            move |_profile| {
                d.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SiteProfileAction::Deleted)
            },
            |_profile| Ok(1),
            |_profile| None, // nothing in the registry derives this name
        )
        .unwrap();

        assert_eq!(report.lifted, 0, "{report:?}");
        assert_eq!(report.kept_unmatched, vec![name.clone()], "{report:?}");
        assert_eq!(deletes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()],
            "the deletion record survives: it is retried, never forgotten"
        );
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
        // And the name is still never handed to an open.
        let queued = read_pending_profile_deletes(&state).unwrap();
        // The stored generation (0) does not derive the tombstone (g1), so an
        // open needs no bump at all: it takes g0 and the owed deletion stays
        // queued. Consuming a generation here would be the blind bump this
        // round removed.
        let (resolved, advanced) = resolve_queued_free_generation(id, 0, &queued).unwrap();
        assert!(!advanced, "an already-free generation must not be consumed");
        assert_ne!(android_profile_name(id, resolved), name);
        // Had the stored generation been the tombstone's own (an older build
        // bumped nothing), the open resolves past it instead of reusing the
        // name whose deletion is still owed.
        let (bumped, advanced) = resolve_queued_free_generation(id, 1, &queued).unwrap();
        assert!(advanced);
        assert!(bumped > 1, "{bumped}");
        assert_ne!(android_profile_name(id, bumped), name);
    }

    /// An orphan (the "clear all" sweep owns no site for it) whose deletion is
    /// confirmed is NOT lifted. With no site id there is no generation to
    /// secure, and "no registered site derives this name today" is not proof
    /// that none ever will: a registry backup can restore the id, whose stored
    /// generation may sit at or below the tombstone's. So the entry becomes a
    /// PERMANENT block, durably, before the ledger is retired — and the
    /// provider is not asked again on later starts.
    #[test]
    fn an_orphan_tombstone_becomes_a_permanent_block_instead_of_lifting() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let name = format!("{ANDROID_PROFILE_PREFIX}{}-g4", "c".repeat(32));
        write_queue(&state, vec![PendingProfileDelete::orphan(&name)]);

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(2)
            },
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 0, "{report:?}");
        assert_eq!(report.blocked, 1, "{report:?}");
        assert!(report.kept_unmatched.is_empty(), "{report:?}");
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::blocked_orphan(&name)],
            "the permanent block is durable and no success path drops it"
        );
        assert_eq!(retires.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// An orphan whose site id was re-registered derives a real site's name
    /// again — so it is secured like an owned tombstone instead of lifted.
    #[test]
    fn an_orphan_tombstone_whose_site_came_back_secures_the_generation() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-orphan-came-back";
        let name = android_profile_name(id, 4);
        write_queue(&state, vec![PendingProfileDelete::orphan(&name)]);
        write_generation(&state, id, 4);

        let ids = vec![id.to_string()];
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Absent),
            |_profile| Ok(0),
            |profile| match_queued_name_to_site(profile, &ids),
        )
        .unwrap();

        assert_eq!(report.lifted, 1, "{report:?}");
        assert_eq!(
            profile_generation(&state, id).unwrap(),
            5,
            "the re-registered site must not derive the drained name"
        );
        assert_ne!(android_profile_name(id, 5), name);
    }

    // ---------------------------------------------------------------------
    // The "clear all" orphan sweep: the third path through the same protocol.
    // ---------------------------------------------------------------------

    /// A name the provider still lists but no registered site owns.
    fn orphan_name(seed: &str, generation: u32) -> String {
        format!("{ANDROID_PROFILE_PREFIX}{seed}-g{generation}")
    }

    /// THE ORDER IS THE SAFETY PROPERTY, asserted as the exact interleaving of
    /// durable writes and injected platform calls: tombstone -> provider ->
    /// permanent block -> ledger.
    #[test]
    fn orphan_sweep_tombstones_before_the_provider_and_blocks_before_retiring() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, _fail_gen, _fail_queue) = ordering_state_with_queue_failures();
        let name = format!("{}{}-g4", ANDROID_PROFILE_PREFIX, "c".repeat(32));
        let names = vec![name.clone()];
        let in_flight = Arc::new(Mutex::new(Vec::<PendingProfileDelete>::new()));
        let seen = in_flight.clone();

        let report = run_orphan_profile_sweep(
            &state,
            &names,
            |profile| {
                // What a kill inside the provider call would leave behind.
                *seen.lock().unwrap() = read_pending_profile_delete_entries(&state).unwrap();
                writes.lock().unwrap().push(format!("delete:{profile}"));
                Ok(SiteProfileAction::Deleted)
            },
            |profile| {
                writes.lock().unwrap().push(format!("retire:{profile}"));
                Ok(1)
            },
        )
        .unwrap();

        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("delete:{name}"),
                format!("put:{PENDING_PROFILE_DELETES_KEY}"),
                format!("retire:{name}"),
            ],
            "tombstone -> provider -> permanent block -> ledger"
        );
        assert_eq!(
            *in_flight.lock().unwrap(),
            vec![PendingProfileDelete::orphan(&name)],
            "the tombstone is durable, as ORPHAN and still OWED, while the provider call is in flight"
        );
        assert_eq!(
            report,
            OrphanSweepReport {
                queued: 1,
                blocked: 1,
                retired: 1,
                ..Default::default()
            },
            "{report:?}"
        );
        // The success path leaves the name blocked, never dropped.
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::blocked_orphan(&name)]
        );
        assert_eq!(read_pending_profile_deletes(&state).unwrap(), vec![name]);
    }

    /// Requirement: a failed tombstone write means the provider is never asked.
    /// An unrecorded deletion is exactly the hole this closes.
    #[test]
    fn orphan_sweep_never_calls_the_provider_when_the_tombstone_write_fails() {
        use crate::android_bridge::SiteProfileAction;

        let (state, writes, _fail_gen, fail_queue) = ordering_state_with_queue_failures();
        fail_queue.store(1, std::sync::atomic::Ordering::SeqCst);
        let names = vec![
            format!("{}{}-g2", ANDROID_PROFILE_PREFIX, "d".repeat(32)),
            format!("{}{}-g3", ANDROID_PROFILE_PREFIX, "e".repeat(32)),
        ];
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();

        let err = run_orphan_profile_sweep(
            &state,
            &names,
            move |_profile| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SiteProfileAction::Deleted)
            },
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("yazılamadı"), "{err}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no provider call before the tombstone is durable"
        );
        assert_eq!(retires.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            read_pending_profile_deletes(&state).unwrap().is_empty(),
            "nothing was recorded, so nothing may be claimed as deleted"
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![format!("put-failed:{PENDING_PROFILE_DELETES_KEY}")]
        );
    }

    /// A bridge error or timeout can arrive AFTER the provider started or
    /// accepted the deletion, so the tombstone must survive it — and the sweep
    /// stops instead of marching on without a bridge.
    #[test]
    fn orphan_sweep_keeps_the_tombstone_when_the_bridge_never_answers() {
        let (state, _writes, _fail_gen, _fail_queue) = ordering_state_with_queue_failures();
        let first = format!("{}{}-g1", ANDROID_PROFILE_PREFIX, "a".repeat(32));
        let second = format!("{}{}-g2", ANDROID_PROFILE_PREFIX, "b".repeat(32));
        let names = vec![first.clone(), second.clone()];
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();

        let err = run_orphan_profile_sweep(
            &state,
            &names,
            move |_profile| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(AppError::Other("köprü hazır değil".into()))
            },
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("köprü hazır değil"), "{err}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the sweep stops at the first dead bridge"
        );
        assert_eq!(
            retires.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no ledger is retired for an unconfirmed deletion"
        );
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::orphan(&first)],
            "the tombstone survives, still OWED, so the startup drain finishes it"
        );
    }

    /// `Pending` means the profile is loaded in this process: kept owed, ledger
    /// kept, and the next start's drain lands it as a permanent block.
    #[test]
    fn orphan_sweep_keeps_a_deferred_orphan_owed_for_the_startup_drain() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let name = orphan_name(&"f".repeat(32), 5);
        let names = vec![name.clone()];
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();

        let report = run_orphan_profile_sweep(
            &state,
            &names,
            |_profile| Ok(SiteProfileAction::Pending),
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap();

        assert_eq!(report.deferred, 1, "{report:?}");
        assert_eq!(report.blocked, 0, "{report:?}");
        assert_eq!(retires.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::orphan(&name)],
            "still owed, never reported as deleted"
        );

        // The next start finishes it: delete, block, retire — in that order.
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let drain = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            |_profile| Ok(1),
            no_owner,
        )
        .unwrap();
        assert_eq!(drain.blocked, 1, "{drain:?}");
        assert_eq!(drain.lifted, 0, "{drain:?}");
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::blocked_orphan(&name)]
        );
    }

    /// `Absent` is a confirmed deletion too: the name becomes unreachable
    /// durably instead of becoming reusable.
    #[test]
    fn orphan_sweep_blocks_a_confirmed_name_for_deleted_and_absent_alike() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let first = orphan_name(&"1".repeat(32), 1);
        let second = orphan_name(&"2".repeat(32), 2);
        let names = vec![first.clone(), second.clone()];
        let answer = first.clone();

        let report = run_orphan_profile_sweep(
            &state,
            &names,
            move |profile| {
                if profile == answer {
                    Ok(SiteProfileAction::Deleted)
                } else {
                    Ok(SiteProfileAction::Absent)
                }
            },
            |_profile| Ok(0),
        )
        .unwrap();

        assert_eq!(report.blocked, 2, "{report:?}");
        assert_eq!(report.retired, 2, "{report:?}");
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![
                PendingProfileDelete::blocked_orphan(&first),
                PendingProfileDelete::blocked_orphan(&second)
            ],
            "both confirmed answers leave a permanent block"
        );
        // The open protocol consults exactly this list, so neither name can be
        // handed to an open again.
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![first, second]
        );
    }

    /// A provider without the profile API deletes nothing: the names stay OWED
    /// (so a later provider update can drain them) and the remaining names are
    /// still tried — a feature gap is not a success and not an abort.
    #[test]
    fn orphan_sweep_keeps_names_owed_when_the_provider_has_no_profile_api() {
        let state = state_with_current_site(None);
        let first = orphan_name(&"3".repeat(32), 1);
        let second = orphan_name(&"4".repeat(32), 2);
        let names = vec![first.clone(), second.clone()];
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();

        let report = run_orphan_profile_sweep(
            &state,
            &names,
            move |_profile| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(AppError::Unsupported("MULTI_PROFILE yok".into()))
            },
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
        )
        .unwrap();

        assert_eq!(report.unsupported, 2, "{report:?}");
        assert_eq!(report.blocked, 0, "{report:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(retires.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![
                PendingProfileDelete::orphan(&first),
                PendingProfileDelete::orphan(&second)
            ]
        );
    }

    /// A permanent block is not re-deleted, not re-retired, not dropped and not
    /// reported as a hazard — across repeated starts.
    #[test]
    fn a_blocked_orphan_is_never_asked_again_and_never_dropped() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let name = orphan_name(&"5".repeat(32), 7);
        write_queue(&state, vec![PendingProfileDelete::blocked_orphan(&name)]);

        for round in 0..3 {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let c = calls.clone();
            let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let r = retires.clone();
            let entries = read_pending_profile_delete_entries(&state).unwrap();
            let report = run_pending_delete_drain(
                &state,
                &entries,
                move |_profile| {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(SiteProfileAction::Deleted)
                },
                move |_profile| {
                    r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(1)
                },
                no_owner,
            )
            .unwrap();

            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "round {round}: a confirmed block is not re-deleted"
            );
            assert_eq!(
                retires.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "round {round}: nothing left to retire"
            );
            assert_eq!(report.blocked, 1, "round {round}: {report:?}");
            assert_eq!(report.lifted, 0, "round {round}: {report:?}");
            assert!(
                report.kept_unmatched.is_empty(),
                "round {round}: a steady-state block is not a hazard: {report:?}"
            );
            assert_eq!(
                read_pending_profile_delete_entries(&state).unwrap(),
                vec![PendingProfileDelete::blocked_orphan(&name)],
                "round {round}: the block survives"
            );
        }
    }

    /// The case "no owner right now" cannot prove away: a registry backup
    /// restores the site id WITHOUT its generation key, so the stored
    /// generation sits below the tombstone's and an open would derive the very
    /// name whose directory was deleted. A blocked entry is therefore still
    /// re-matched, and lifted only after the generation is durable.
    #[test]
    fn a_blocked_orphan_whose_site_id_comes_back_secures_a_new_generation() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-restored-from-backup";
        let name = android_profile_name(id, 4);
        write_queue(&state, vec![PendingProfileDelete::blocked_orphan(&name)]);
        write_generation(&state, id, 0);

        let ids = vec![id.to_string()];
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            move |_profile| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(SiteProfileAction::Deleted)
            },
            |_profile| Ok(1),
            |profile| match_queued_name_to_site(profile, &ids),
        )
        .unwrap();

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "already confirmed: the provider is not asked again"
        );
        assert_eq!(report.lifted, 1, "{report:?}");
        assert_eq!(report.blocked, 0, "{report:?}");
        assert_eq!(
            profile_generation(&state, id).unwrap(),
            5,
            "the restored site must not derive the blocked name"
        );
        assert!(
            read_pending_profile_deletes(&state).unwrap().is_empty(),
            "lifted only after the generation was durable"
        );
        // And the next open of the restored site derives a DIFFERENT name.
        let stored = profile_generation(&state, id).unwrap();
        let queued = read_pending_profile_deletes(&state).unwrap();
        let (resolved, advanced) = resolve_queued_free_generation(id, stored, &queued).unwrap();
        assert!(!advanced, "nothing is queued, nothing to step over");
        assert_eq!(resolved, 5);
        assert_ne!(android_profile_name(id, resolved), name);
    }

    /// Requirement: a failed durable write keeps the tombstone. For an orphan
    /// the block IS the durable record, so a refused block write leaves the
    /// entry owed (and the ledger untouched) until a later start lands it.
    #[test]
    fn a_failed_block_write_keeps_the_orphan_tombstone_owed() {
        use crate::android_bridge::SiteProfileAction;

        let (state, _writes, _fail_gen, fail_queue) = ordering_state_with_queue_failures();
        let name = orphan_name(&"9".repeat(32), 9);
        write_queue(&state, vec![PendingProfileDelete::orphan(&name)]);
        fail_queue.store(1, std::sync::atomic::Ordering::SeqCst);
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = retires.clone();

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            move |_profile| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            },
            no_owner,
        )
        .unwrap();

        assert_eq!(report.lifted, 0, "{report:?}");
        assert_eq!(report.blocked, 0, "{report:?}");
        assert_eq!(report.kept_unmatched, vec![name.clone()], "{report:?}");
        assert_eq!(
            retires.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no ledger retirement without the durable block"
        );
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::orphan(&name)],
            "still owed, still keeping the name out of every open"
        );

        // The next start, with the store writable again, lands the block.
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Absent),
            |_profile| Ok(0),
            no_owner,
        )
        .unwrap();
        assert_eq!(report.blocked, 1, "{report:?}");
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::blocked_orphan(&name)]
        );
    }

    /// Repeated sweeps of the same stale name and repeated drains consume no
    /// generation and change nothing: the block is idempotent.
    #[test]
    fn repeated_orphan_sweeps_and_drains_consume_no_generation() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-orphan-idempotent";
        let name = android_profile_name(id, 3);
        let names = vec![name.clone()];
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retires = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for round in 0..3 {
            let c = calls.clone();
            let r = retires.clone();
            let report = run_orphan_profile_sweep(
                &state,
                &names,
                move |_profile| {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(SiteProfileAction::Absent)
                },
                move |_profile| {
                    r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(0)
                },
            )
            .unwrap();
            assert_eq!(report.blocked, 1, "round {round}: {report:?}");
            assert_eq!(
                read_pending_profile_delete_entries(&state).unwrap(),
                vec![PendingProfileDelete::blocked_orphan(&name)],
                "round {round}: exactly one entry, blocked, never duplicated"
            );
            assert_eq!(
                profile_generation(&state, id).unwrap(),
                0,
                "round {round}: an orphan never touches a generation"
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(retires.load(std::sync::atomic::Ordering::SeqCst), 3);

        // The startup drain asks nothing more of it, and still consumes nothing.
        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            |_profile| Ok(1),
            |profile| match_queued_name_to_site(profile, &[]),
        )
        .unwrap();
        assert_eq!(report.blocked, 1, "{report:?}");
        assert_eq!(report.lifted, 0, "{report:?}");
        assert_eq!(profile_generation(&state, id).unwrap(), 0);
    }

    /// A recorded owner that contradicts the registry is ambiguous: keep the
    /// tombstone rather than pick one of the two.
    #[test]
    fn a_conflicting_owner_keeps_the_tombstone() {
        use crate::android_bridge::SiteProfileAction;

        let state = state_with_current_site(None);
        let id = "user-conflict";
        let name = android_profile_name(id, 0);
        write_queue(&state, vec![PendingProfileDelete::site(id, &name)]);

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let report = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Ok(SiteProfileAction::Deleted),
            |_profile| Ok(1),
            |_profile| Some("a-completely-different-site".to_string()),
        )
        .unwrap();

        assert_eq!(report.lifted, 0, "{report:?}");
        assert_eq!(report.kept_unmatched, vec![name.clone()], "{report:?}");
        assert_eq!(read_pending_profile_deletes(&state).unwrap(), vec![name]);
    }

    /// A provider without the profile API keeps every tombstone and still tries
    /// the remaining names; a bridge that is not ready stops the round and asks
    /// for a retry.
    #[test]
    fn drain_survives_an_unsupported_provider_and_an_unready_bridge() {
        let state = state_with_current_site(None);
        let first = "user-unsupported-a";
        let second = "user-unsupported-b";
        let name_a = android_profile_name(first, 0);
        let name_b = android_profile_name(second, 1);
        write_queue(
            &state,
            vec![
                PendingProfileDelete::site(first, &name_a),
                PendingProfileDelete::site(second, &name_b),
            ],
        );

        let entries = read_pending_profile_delete_entries(&state).unwrap();
        let unsupported = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Err(AppError::Unsupported("MULTI_PROFILE yok".into())),
            |_profile| Ok(1),
            no_owner,
        )
        .unwrap();
        assert_eq!(unsupported.unsupported, 2, "{unsupported:?}");
        assert_eq!(unsupported.lifted, 0);
        assert!(!unsupported.bridge_not_ready);
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name_a.clone(), name_b.clone()],
            "a feature gap must not strand or drop the owed deletions"
        );

        let not_ready = run_pending_delete_drain(
            &state,
            &entries,
            |_profile| Err(AppError::Other("köprü hazır değil".into())),
            |_profile| Ok(1),
            no_owner,
        )
        .unwrap();
        assert!(not_ready.bridge_not_ready, "{not_ready:?}");
        assert_eq!(not_ready.lifted, 0);
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name_a, name_b],
            "nothing is lifted when the bridge never answered"
        );
    }

    /// The pure generation rule: monotone, idempotent, owner-checked and
    /// overflow-safe.
    #[test]
    fn generation_past_name_is_monotone_idempotent_and_overflow_safe() {
        let id = "user-generation-rule";
        let name = android_profile_name(id, 3);

        // Stored behind the tombstone → advance to g+1.
        assert_eq!(generation_past_name(id, 3, &name).unwrap(), (4, true));
        assert_eq!(generation_past_name(id, 0, &name).unwrap(), (4, true));
        // Stored already past it → no bump, no generation burned.
        assert_eq!(generation_past_name(id, 4, &name).unwrap(), (4, false));
        assert_eq!(generation_past_name(id, 9, &name).unwrap(), (9, false));
        // Another site's tombstone must never move this counter.
        let foreign = android_profile_name("another-site", 3);
        assert!(generation_past_name(id, 0, &foreign).is_err());
        // Neither does a name that merely shares the prefix shape.
        assert!(generation_past_name(id, 0, "site-not-a-profile").is_err());
        assert!(generation_past_name(id, 0, "").is_err());
        // u32::MAX has no next value: refuse instead of making the profile
        // reachable again.
        let maxed = android_profile_name(id, u32::MAX);
        let err = generation_past_name(id, u32::MAX, &maxed).unwrap_err();
        assert!(matches!(err, AppError::Storage(_)), "{err}");
        assert!(err.to_string().contains("taştı"), "{err}");
        // A stored generation past MAX cannot exist, but the rule still holds.
        assert_eq!(
            generation_past_name(id, u32::MAX, &name).unwrap(),
            (u32::MAX, false)
        );
    }

    /// Owner re-matching needs EXACTLY one registered site: none means
    /// unmatched, two means ambiguous — both refuse to guess.
    #[test]
    fn match_queued_name_to_site_needs_exactly_one_owner() {
        let id = "user-match";
        let name = android_profile_name(id, 7);
        assert_eq!(
            match_queued_name_to_site(&name, &[id.to_string()]).as_deref(),
            Some(id)
        );
        assert_eq!(
            match_queued_name_to_site(&name, &["other".to_string(), id.to_string()]).as_deref(),
            Some(id)
        );
        assert_eq!(match_queued_name_to_site(&name, &[]), None);
        assert_eq!(
            match_queued_name_to_site(&name, &["other".to_string()]),
            None
        );
        // A different generation of the same site still matches (the owner is
        // the site, not the generation).
        assert_eq!(
            match_queued_name_to_site(&android_profile_name(id, 0), &[id.to_string()]).as_deref(),
            Some(id)
        );
        // Two ids deriving the same name would be a digest collision: refuse.
        let collide = format!("{ANDROID_PROFILE_PREFIX}{}-g1", "d".repeat(32));
        assert_eq!(match_queued_name_to_site(&collide, &[]), None);
        // Names outside the namespace never match, however close they look.
        assert_eq!(
            match_queued_name_to_site("Default", &[id.to_string()]),
            None
        );
        assert_eq!(match_queued_name_to_site("", &[id.to_string()]), None);
    }

    /// Queueing records the owner, dedupes by name, and upgrades a legacy
    /// entry once a path knows whose profile it is.
    #[test]
    fn queue_records_the_owner_and_upgrades_a_legacy_entry() {
        let state = state_with_current_site(None);
        let id = "user-queue-owner";
        let name = android_profile_name(id, 0);

        // Legacy bytes → unknown owner.
        write_legacy_queue(&state, vec![name.clone()]);
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::unknown(&name)]
        );

        // Queueing the same name for a known site upgrades the owner (and the
        // stored bytes become the current format).
        queue_site_profile_delete(&state, id, &name).unwrap();
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::site(id, &name)]
        );
        // Dedupe by name: queueing again writes nothing new.
        queue_site_profile_delete(&state, id, &name).unwrap();
        assert_eq!(
            read_pending_profile_deletes(&state).unwrap(),
            vec![name.clone()]
        );
        // A known owner is never downgraded or overwritten by another claim.
        queue_orphan_profile_delete(&state, &name).unwrap();
        assert_eq!(
            read_pending_profile_delete_entries(&state).unwrap(),
            vec![PendingProfileDelete::site(id, &name)]
        );
        // The name-only wrapper still sees a plain list.
        queue_pending_profile_delete(&state, &android_profile_name("other", 1)).unwrap();
        assert_eq!(read_pending_profile_deletes(&state).unwrap().len(), 2);
    }

    #[test]
    fn pending_without_a_jar_still_records_the_flag_before_the_view() {
        // A fresh install has the flag missing and no jar: phase 1 runs with
        // an empty cookie list (Kotlin treats that as nothing-to-import),
        // the flag becomes durable, and only then does the view exist.
        let (state, fail_flag, _writes) = failing_flag_state();
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let id = "user-nojar";

        let mut ops = ScriptedOpenOps::ok();
        let opened =
            run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops).unwrap();
        let profile = android_profile_name(id, 0);
        assert_eq!(opened, profile);
        assert_eq!(
            ops.calls,
            vec![format!("migrate:{profile}"), format!("open:{profile}")]
        );
        assert_eq!(ops.cookies_seen, vec![0]);
        assert!(state
            .provider
            .get_secret(&format!("profilemig-{id}"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn expired_jar_cookies_never_ride_the_migration() {
        // Only live cookies are handed to phase 1 — an expired snapshot must
        // not be resurrected into a virgin profile.
        let (state, fail_flag, _writes) = failing_flag_state();
        fail_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let id = "user-expired";
        let jar = CookieJar {
            site_id: "jar-site".into(),
            saved_at: crate::anilist::now_unix(),
            cookies: vec![crate::web::session::StoredCookie {
                name: "old".into(),
                value: "x".into(),
                domain: None,
                path: None,
                expires: Some(crate::anilist::now_unix() - 10),
                secure: false,
                http_only: false,
                same_site: None,
            }],
        };
        state
            .provider
            .put_secret(&format!("cookies-{id}"), &serde_json::to_vec(&jar).unwrap())
            .unwrap();

        let mut ops = ScriptedOpenOps::ok();
        run_mobile_open_protocol(&state, id, "https://a.example/", "", &mut ops).unwrap();
        assert_eq!(ops.cookies_seen, vec![0], "expired cookies are dropped");
    }
}
