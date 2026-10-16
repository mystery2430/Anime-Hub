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

/// Secret key where the RAW bytes of a corrupt pending-delete list are
/// kept before the working copy is salvaged/overwritten — the names inside
/// are the only record of on-disk profile data still awaiting deletion.
pub const CORRUPT_QUEUE_BACKUP_KEY: &str = "android-pending-profile-deletes-corrupt";

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
        match std::str::from_utf8(&raw[i..end]) {
            Some(name) if is_valid_site_profile_name(name) => {
                found.push(name.to_string());
                i = end;
            }
            _ => i += 1,
        }
    }
    found
}

/// Pure recovery rule for the pending-delete queue. A JSON list that parses
/// keeps only structurally valid names (corrupt if any were dropped); a
/// broken document is salvaged name-by-name from the raw bytes (always
/// corrupt). Returns `(names, was_corrupt)`.
pub fn recover_pending_profile_deletes(raw: &[u8]) -> (Vec<String>, bool) {
    match serde_json::from_slice::<Vec<String>>(raw) {
        Ok(list) => {
            let total = list.len();
            let kept: Vec<String> = list
                .into_iter()
                .filter(|name| is_valid_site_profile_name(name))
                .collect();
            let dropped = total - kept.len();
            if dropped > 0 {
                log::warn!("bekleyen profil silme listesinden {dropped} geçersiz ad çıkarıldı");
            }
            (kept, dropped > 0)
        }
        Err(e) => {
            let salvaged = scan_valid_profile_names(raw);
            log::warn!(
                "bekleyen profil silme listesi bozuk ({e}); {} ad ham kopyadan kurtarıldı",
                salvaged.len()
            );
            (salvaged, true)
        }
    }
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

/// Read the deferred profile-deletion queue.
///
/// A corrupt list is never treated as empty: valid names are salvaged from
/// the raw bytes, the original bytes are backed up under
/// [`CORRUPT_QUEUE_BACKUP_KEY`], and the working copy is repaired in place.
/// Dropping the queue silently would strand those profile deletions (best
/// case the orphan sweep collects them — worst case a re-open races a
/// deletion nobody recorded any more).
pub fn read_pending_profile_deletes(state: &AppState) -> AppResult<Vec<String>> {
    let Some(bytes) = state.provider.get_secret(PENDING_PROFILE_DELETES_KEY)? else {
        return Ok(Vec::new());
    };
    let (list, corrupt) = recover_pending_profile_deletes(&bytes);
    if corrupt {
        // Best effort both ways: a failed backup or repair only means the
        // salvage repeats on the next read; never block open behind it.
        if let Err(e) = state.provider.put_secret(CORRUPT_QUEUE_BACKUP_KEY, &bytes) {
            log::warn!("bozuk silme kuyruğu yedeği yazılamadı: {e}");
        }
        match serde_json::to_vec(&list) {
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
    Ok(list)
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
                if let Some(cookie) = restore(stored) {
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
                queue_pending_profile_delete(state, &profile)?;
                let retry_generation = bump_profile_generation(state, site_id)?;
                profile = android_profile_name(site_id, retry_generation);
                migrate_result = ops.migrate(url, &profile, &cookie_headers);
                if let Err(AppError::Other(retry_message)) = &migrate_result {
                    if retry_message.contains(animehub_android::MIGRATION_RETRY_SENTINEL) {
                        // Retire this attempt too and surface a retryable error.
                        if let Err(e) = queue_pending_profile_delete(state, &profile) {
                            log::warn!("ikinci göç profili kuyruğa alınamadı: {e}");
                        }
                        if let Err(e) = bump_profile_generation(state, site_id) {
                            log::warn!("göç sonrası nesil artırılamadı: {e}");
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
    }

    impl ScriptErr {
        fn into_app(self) -> AppError {
            match self {
                ScriptErr::Other(message) => AppError::Other(message),
                ScriptErr::Unsupported => {
                    AppError::Unsupported("sağlayıcı çoklu profili desteklemiyor".into())
                }
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
        fn backend(&self) -> crate::secure::Backend {
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
