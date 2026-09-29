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
use crate::web::session::{capture, profile_dir_name, CookieJar};
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
            window_label: label.clone(),
            entered_fullscreen,
        };
        *state.current_site.lock().expect("current_site lock") = Some(value);
    }

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
// Mobile: a single WebView, isolated by swapping cookies + localStorage +
// IndexedDB (the platform has no per-profile data directory).
// ---------------------------------------------------------------------------

#[cfg(mobile)]
fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {
    let window = app
        .get_webview_window(LAUNCHER_LABEL)
        .ok_or_else(|| AppError::Other("ana WebView bulunamadı".into()))?;

    let state = app.state::<AppState>();

    // 1. Export the outgoing site's cookies and web storage while its page is
    //    still the loaded origin — `localStorage`/IndexedDB reads must happen
    //    before anything is cleared or navigated. The storage export is
    //    best-effort and never blocks the switch.
    if let Some(current) = state
        .current_site
        .lock()
        .expect("current_site lock")
        .clone()
    {
        export_cookies(&window, &current.site_id)?;
        export_site_storage(app, &current.site_id);
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
        window_label: LAUNCHER_LABEL.into(),
        entered_fullscreen: false,
    });

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
    let watcher_host = site.host();
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
                .is_some_and(|u| u.host_str() == Some(watcher_host.as_str()));
            if loaded {
                break;
            }
        }
        import_site_storage(&watcher_app, &watcher_site_id);
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
fn export_site_storage(app: &AppHandle, site_id: &str) {
    let state = app.state::<AppState>();
    let purpose = format!("site-{site_id}");
    for (kind, result) in [
        ("localstorage", crate::android_bridge::localstorage_export(&purpose)),
        ("indexeddb", crate::android_bridge::indexeddb_export(&purpose)),
    ] {
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
/// Refuses to run unless `site_id` is still the current session, so a stale
/// watcher can never write one site's state into another site's origin.
#[cfg(mobile)]
fn import_site_storage(app: &AppHandle, site_id: &str) {
    {
        let state = app.state::<AppState>();
        let current = state.current_site.lock().expect("current_site lock");
        if !current.as_ref().is_some_and(|c| c.site_id == site_id) {
            return;
        }
    }
    let state = app.state::<AppState>();
    let purpose = format!("site-{site_id}");
    for kind in ["localstorage", "indexeddb"] {
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
        export_site_storage(app, &current.site_id);
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
}
