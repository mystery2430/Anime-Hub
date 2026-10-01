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
//! The system WebView has no per-profile data-directory API, so the main
//! WebView is navigated with a full state swap: cookies are exported/imported
//! as before, and `localStorage` / IndexedDB are additionally exported into
//! Keystore-sealed blobs on leave and restored after the target page loads
//! (storage is origin-scoped, so the restore must wait for the page).

use crate::commands::{AppState, CurrentSite};
use crate::error::{AppError, AppResult};
use crate::sites::registry::Site;
#[cfg(desktop)]
use crate::web::dns::{classify_host, DnsClass};
// `capture`/`CookieJar` back the shared export path (mobile jar swap and
// desktop child-webview snapshot); `restore` only serves the mobile import.
#[cfg(mobile)]
use crate::web::session::restore;
use crate::web::session::{capture, origin_key, profile_dir_name, CookieJar};
#[cfg(mobile)]
use crate::web::session::url_has_origin;
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

/// How often a live site session is snapshotted into the encrypted store.
///
/// The close-time export remains the authoritative final write; this interval
/// only bounds what a crash (or an Android process kill, which skips the
/// close path entirely) can lose.
const SESSION_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Safety valve: a sync thread ends itself after this long even if the
/// ownership checks somehow never fire.
const SESSION_SYNC_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(12 * 3600);

/// Android storage dumps are much heavier than a cookie read (a full
/// IndexedDB scan on the page), so they run every Nth cookie tick rather than
/// every tick.
#[cfg(mobile)]
const STORAGE_SYNC_EVERY_N_TICKS: u32 = 10;

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
    // (saving its jar) before opening the next.
    close_current_child(app)?;

    // Per-site profile directory: WebKitGTK's data directory on Linux, and on
    // Windows the WebView2 user-data folder. This is what makes cookies,
    // localStorage, IndexedDB and cache independent per site.
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| {
            log::error!("uygulama veri dizini alınamadı: {e}");
            AppError::Storage("uygulama veri dizini alınamadı".into())
        })?
        .join("profiles")
        .join(profile_dir_name(&site.id));
    std::fs::create_dir_all(&data_dir)?;

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
            origin: origin_key(url),
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
/// if any. Used before hosting a new one.
#[cfg(desktop)]
fn close_current_child(app: &AppHandle) -> AppResult<()> {
    let label = {
        let state = app.state::<AppState>();
        let current = state.current_site.lock().expect("current_site lock");
        current
            .as_ref()
            .filter(|c| c.window_label != LAUNCHER_LABEL)
            .map(|c| c.window_label.clone())
    };
    match label {
        Some(label) => close_site_window(app, &label),
        None => Ok(()),
    }
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
#[cfg(any(desktop, test))]
fn session_is_current(state: &AppState, site_id: &str) -> bool {
    let current = state.current_site.lock().expect("current_site lock");
    current.as_ref().is_some_and(|c| c.site_id == site_id)
}

/// Mobile storage belongs to a specific site *and* origin, not just a host.
/// A different scheme/port on the same hostname must not receive the blob.
#[cfg(any(mobile, test))]
fn session_is_current_origin(state: &AppState, site_id: &str, expected_origin: &str) -> bool {
    let current = state.current_site.lock().expect("current_site lock");
    current
        .as_ref()
        .is_some_and(|c| c.site_id == site_id && c.origin == expected_origin)
}

/// Snapshot the live session into the encrypted store on a fixed interval.
///
/// Desktop variant: cookies only — localStorage/IndexedDB already live in the
/// per-site profile directory that the WebView itself persists. The loop
/// exits on the first tick after the session ends (site closed, another site
/// opened, or the child webview is gone without a close event).
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

/// Mobile variant: cookies every tick, and localStorage/IndexedDB every
/// [`STORAGE_SYNC_EVERY_N_TICKS`] ticks.
///
/// Android can kill the process without ever running the close path, so the
/// blobs must stay fresh while the session lives. Every write is guarded by
/// both session ownership and a loaded-origin check: storage reads only make
/// sense while the site's own page is the loaded document, and skipping the
/// tick otherwise costs nothing.
#[cfg(mobile)]
fn spawn_session_sync(
    app: AppHandle,
    _label: String,
    site_id: String,
    host: String,
    expected_origin: String,
) {
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let Some(window) = app.get_webview_window(LAUNCHER_LABEL) else {
            return;
        };
        let mut tick: u32 = 0;
        loop {
            std::thread::sleep(SESSION_SYNC_INTERVAL);
            if started.elapsed() > SESSION_SYNC_MAX_LIFETIME {
                log::warn!("oturum eşitlemesi ömrünü doldurdu: {site_id}");
                return;
            }
            {
                let state = app.state::<AppState>();
                if !session_is_current_origin(&state, &site_id, &expected_origin) {
                    return;
                }
            }
            let current_url = window.url().ok();
            let loaded = current_url
                .as_ref()
                .is_some_and(|u| u.host_str() == Some(host.as_str()));
            if !loaded {
                continue;
            }
            tick = tick.wrapping_add(1);
            if let Err(e) = export_cookies(&window, &site_id) {
                log::warn!("dönemsel çerez eşitlemesi başarısız ({site_id}): {e}");
            }
            if tick % STORAGE_SYNC_EVERY_N_TICKS == 0
                && current_url
                    .as_ref()
                    .is_some_and(|u| url_has_origin(u, &expected_origin))
            {
                export_site_storage(&app, &site_id, &expected_origin);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Mobile: a single WebView, isolated by swapping cookies + localStorage +
// IndexedDB (the platform has no per-profile data directory).
// ---------------------------------------------------------------------------

#[cfg(mobile)]
fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {
    let window = app
        .get_webview_window(LAUNCHER_LABEL)
        .ok_or_else(|| AppError::Other("ana WebView bulunamadı".into()))?;

    let state = app.state::<AppState>();
    let expected_origin = origin_key(url);

    // 1. Export the outgoing site's cookies and origin-scoped storage while
    //    its page is still loaded. Cookie snapshots remain site-scoped; the
    //    localStorage/IndexedDB blob is stricter and is written only if the
    //    exact scheme/host/port still matches the registered origin.
    // Bind the clone before entering the body so the MutexGuard is dropped
    // before storage export re-checks ownership through `current_site`.
    let outgoing = state
        .current_site
        .lock()
        .expect("current_site lock")
        .clone();
    if let Some(current) = outgoing {
        export_cookies(&window, &current.site_id)?;
        export_site_storage(app, &current.site_id, &current.origin);
    }

    // 2. Clear the shared jar, then load the target site's cookies.
    window.clear_all_browsing_data().map_err(|e| {
        log::error!("çerezler temizlenemedi: {e}");
        AppError::Other("çerezler temizlenemedi".into())
    })?;
    import_cookies(&window, &site.id)?;

    // 3. Install the cosmetic/anti-popup script and navigate.
    window.eval(init_script).map_err(|e| {
        log::error!("script yüklenemedi: {e}");
        AppError::Other("script yüklenemedi".into())
    })?;

    window.navigate(url.clone()).map_err(|e| {
        log::error!("sayfa açılamadı: {e}");
        AppError::Other("sayfa açılamadı".into())
    })?;

    *state.current_site.lock().expect("current_site lock") = Some(CurrentSite {
        site_id: site.id.clone(),
        host: site.host(),
        origin: expected_origin.clone(),
        window_label: LAUNCHER_LABEL.into(),
        entered_fullscreen: false,
    });

    // Live-session sync: Android may kill the process without running the
    // close path, so the blobs are refreshed on an interval while open.
    spawn_session_sync(
        app.clone(),
        LAUNCHER_LABEL.to_string(),
        site.id.clone(),
        site.host(),
        expected_origin.clone(),
    );

    // 4. Restore the target site's web storage once its page is loaded.
    //    `localStorage` is origin-scoped: writing before the site page is the
    //    loaded document would land in the launcher's origin. A small watcher
    //    thread polls the WebView URL (sites open rarely; a dedicated thread
    //    keeps this off the async runtime) and then runs the blocking
    //    restore. `import_site_storage` re-checks that the session is still
    //    this site's, so a watcher left behind by a quick close can never
    //    write into another site's origin.
    let watcher_app = app.clone();
    let watcher_window = window;
    let watcher_site_id = site.id.clone();
    let watcher_origin = expected_origin.clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if std::time::Instant::now() >= deadline {
                log::warn!("site deposu geri yükleme zaman aşımı: {watcher_site_id}");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            let loaded = watcher_window
                .url()
                .ok()
                .is_some_and(|u| url_has_origin(&u, &watcher_origin));
            if loaded {
                break;
            }
        }
        import_site_storage(&watcher_app, &watcher_site_id, &watcher_origin);
    });

    Ok(LAUNCHER_LABEL.to_string())
}

/// Export the live site's `localStorage` and IndexedDB into per-site sealed
/// secrets.
///
/// Best-effort by design: a failed export is logged and costs the site its
/// stored state on the next visit, but never blocks closing or switching.
/// Must run while the site page is still the loaded origin.
#[cfg(mobile)]
fn export_site_storage(app: &AppHandle, site_id: &str, expected_origin: &str) {
    let Some(window) = app.get_webview_window(LAUNCHER_LABEL) else {
        return;
    };
    let purpose = format!("site-{site_id}");
    for kind in ["localstorage", "indexeddb"] {
        // Re-check ownership and the actual page before each bridge call: a
        // redirect or a rapid site switch must not snapshot another origin.
        let state = app.state::<AppState>();
        if !session_is_current_origin(&state, site_id, expected_origin)
            || !window
                .url()
                .ok()
                .is_some_and(|u| url_has_origin(&u, expected_origin))
        {
            return;
        }
        let result = if kind == "localstorage" {
            crate::android_bridge::localstorage_export(&purpose)
        } else {
            crate::android_bridge::indexeddb_export(&purpose)
        };
        match result {
            // An empty export means the page had no storage yet — storing an
            // empty blob would only add noise, skip it.
            Ok(sealed) if !sealed.is_empty() => {
                let name = format!("{kind}-{site_id}");
                if let Err(e) = state.provider.put_secret(&name, sealed.as_bytes()) {
                    log::warn!("{kind} blob'u kaydedilemedi ({site_id}): {e}");
                }
            }
            Ok(_) => {}
            Err(e) => log::warn!("{kind} dışa aktarılamadı ({site_id}): {e}"),
        }
    }
}

/// Restore a site's `localStorage` and IndexedDB into the currently loaded
/// page.
///
/// Refuses to run unless both the site and its exact scheme/host/port origin
/// still own the live WebView, so a stale watcher cannot restore a blob into a
/// sibling origin on the same hostname.
#[cfg(mobile)]
fn import_site_storage(app: &AppHandle, site_id: &str, expected_origin: &str) {
    let Some(window) = app.get_webview_window(LAUNCHER_LABEL) else {
        return;
    };
    let purpose = format!("site-{site_id}");
    for kind in ["localstorage", "indexeddb"] {
        let state = app.state::<AppState>();
        if !session_is_current_origin(&state, site_id, expected_origin)
            || !window
                .url()
                .ok()
                .is_some_and(|u| url_has_origin(&u, expected_origin))
        {
            return;
        }
        let sealed = match state.provider.get_secret(&format!("{kind}-{site_id}")) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("{kind} blob'u okunamadı ({site_id}): {e}");
                continue;
            }
        };
        let Ok(sealed) = String::from_utf8(sealed.to_vec()) else {
            log::warn!("{kind} blob'u bozuk ({site_id})");
            continue;
        };
        if sealed.is_empty() {
            continue;
        }
        let result = if kind == "localstorage" {
            crate::android_bridge::localstorage_import(&purpose, &sealed)
        } else {
            crate::android_bridge::indexeddb_import(&purpose, &sealed)
        };
        if let Err(e) = result {
            log::warn!("{kind} geri yüklenemedi ({site_id}): {e}");
        }
    }
}

/// Save the WebView's current cookie jar into the named site's encrypted blob.
#[cfg(mobile)]
fn export_cookies(window: &tauri::WebviewWindow, site_id: &str) -> AppResult<()> {
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

fn export_cookies_from(
    live: Vec<cookie::Cookie<'static>>,
    app: &AppHandle,
    site_id: &str,
) -> AppResult<()> {
    let state = app.state::<AppState>();
    let now = crate::anilist::now_unix();

    let mut jar = CookieJar {
        site_id: site_id.to_string(),
        cookies: live.iter().map(capture).collect(),
        saved_at: now,
    };
    jar.prune_expired(now);

    let bytes = serde_json::to_vec(&jar)?;
    state
        .provider
        .put_secret(&format!("cookies-{site_id}"), &bytes)
}

/// Load a site's encrypted cookie jar into the WebView.
#[cfg(mobile)]
fn import_cookies(window: &tauri::WebviewWindow, site_id: &str) -> AppResult<()> {
    let app = window.app_handle().clone();
    let state = app.state::<AppState>();

    let Some(bytes) = state.provider.get_secret(&format!("cookies-{site_id}"))? else {
        return Ok(());
    };
    let jar: CookieJar = serde_json::from_slice(&bytes)?;
    let now = crate::anilist::now_unix();

    for stored in jar.cookies.iter().filter(|c| c.is_live(now)) {
        if let Some(cookie) = restore(stored) {
            // A single failed cookie must not abort the whole restore.
            if let Err(e) = window.set_cookie(cookie) {
                log::debug!("cookie restore failed for {site_id}: {e}");
            }
        }
    }
    Ok(())
}

/// Close a site session, persisting its cookies and web storage first.
///
/// Mobile: save the jar and the sealed localStorage/IndexedDB blobs (the site
/// page is still loaded here, so the storage export reads the right origin),
/// then send the single WebView back to the launcher.
#[cfg(mobile)]
pub fn close_site_window(app: &AppHandle, _label: &str) -> AppResult<()> {
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

    if let Some(current) = &current {
        let window = app
            .get_webview_window(LAUNCHER_LABEL)
            .ok_or_else(|| AppError::Other("ana WebView bulunamadı".into()))?;
        export_cookies(&window, &current.site_id)?;
        // The live page is still this site's origin; after the navigate below
        // the storage would be unreachable until the next visit.
        export_site_storage(app, &current.site_id, &current.origin);
    }
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

/// Close a site session, persisting its cookies first.
///
/// Desktop: remove the child webview; the launcher underneath reappears.
/// Uses `Webview::close` / `set_fullscreen`, which do not exist on mobile.
#[cfg(desktop)]
pub fn close_site_window(app: &AppHandle, label: &str) -> AppResult<()> {
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
            origin: "https://a.example".into(),
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
    fn storage_writes_are_bound_to_the_current_site_origin() {
        let state = state_with_current_site(Some("s1"));
        assert!(session_is_current_origin(
            &state,
            "s1",
            "https://a.example"
        ));
        assert!(!session_is_current_origin(
            &state,
            "s1",
            "https://a.example:8443"
        ));
        assert!(!session_is_current_origin(
            &state,
            "s1",
            "http://a.example"
        ));
        assert!(!session_is_current_origin(
            &state,
            "s2",
            "https://a.example"
        ));
    }

    #[test]
    fn sync_intervals_are_sane() {
        // A crash loses at most one interval of session changes; keep that
        // bounded but not chatty.
        assert!(SESSION_SYNC_INTERVAL.as_secs() >= 15);
        assert!(SESSION_SYNC_INTERVAL.as_secs() <= 300);
        // The safety valve must always outlive many intervals.
        assert!(SESSION_SYNC_MAX_LIFETIME > SESSION_SYNC_INTERVAL * 100);
        #[cfg(mobile)]
        {
            assert!(STORAGE_SYNC_EVERY_N_TICKS >= 2, "storage dumps are heavy");
        }
    }
}
