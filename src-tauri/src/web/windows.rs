//! WebView window creation and per-site session isolation.
//!
//! This is the only place a site URL is handed to a WebView, and the only
//! place the navigation policy is attached, so the HTTPS/blocklist rules
//! cannot be bypassed by adding a new entry point.

use crate::commands::{AppState, CurrentSite};
use crate::error::{AppError, AppResult};
use crate::sites::registry::Site;
use crate::web::session::{
    capture, decide_navigation, profile_dir_name, restore, CookieJar, NavDecision,
};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use url::Url;

/// Label of the launcher window.
pub const LAUNCHER_LABEL: &str = "main";

/// Prefix for site window labels.
const SITE_LABEL_PREFIX: &str = "site-";

/// Window label used for a site session.
pub fn label_for(site_id: &str) -> String {
    format!("{SITE_LABEL_PREFIX}{}", profile_dir_name(site_id))
}

/// Open a site.
///
/// Returns the window label that now hosts the site.
pub fn open_site_window(
    app: AppHandle,
    site: &Site,
    url: &Url,
    init_script: &str,
    fullscreen: bool,
) -> AppResult<String> {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        open_on_mobile(&app, site, url, init_script)
    } else {
        open_on_desktop(app, site, url, init_script, fullscreen)
    }
}

// ---------------------------------------------------------------------------
// Desktop: one window per site, each with its own profile directory.
// ---------------------------------------------------------------------------

fn open_on_desktop(
    app: AppHandle,
    site: &Site,
    url: &Url,
    init_script: &str,
    fullscreen: bool,
) -> AppResult<String> {
    let label = label_for(&site.id);

    // Re-focus an already-open session instead of stacking windows.
    if let Some(existing) = app.get_webview_window(&label) {
        existing
            .set_focus()
            .map_err(|e| AppError::Other(e.to_string()))?;
        return Ok(label);
    }

    // Per-site profile directory: WebKitGTK's data directory on Linux, and on
    // Windows the WebView2 user-data folder. This is what makes cookies,
    // localStorage, IndexedDB and cache independent per site.
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::Storage(e.to_string()))?
        .join("profiles")
        .join(profile_dir_name(&site.id));
    std::fs::create_dir_all(&data_dir)?;

    let host = site.host();
    let nav_host = host.clone();
    // A separate clone for the navigation closure: the builder borrows `app`
    // for its own lifetime, so the closure cannot also move out of it.
    let nav_app = app.clone();

    // The builder stores the manager for the window's lifetime, so it
    // needs an owned handle; `AppHandle` is a cheap refcounted clone.
    let window = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url.clone()))
        .title(site.name.clone())
        .data_directory(data_dir)
        .initialization_script(init_script)
        .visible(true)
        .focused(true)
        .fullscreen(fullscreen)
        // A site must never be able to talk to the Rust core. Tauri already
        // withholds IPC from external origins; this keeps the intent explicit
        // and blocks any future config drift.
        .accept_first_mouse(true)
        .on_navigation(move |target: &Url| {
            let app = nav_app.clone();
            let decision = {
                let state = app.state::<AppState>();
                let bl = state.blocklist.lock().expect("blocklist lock");
                decide_navigation(target, &nav_host, &bl)
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
        .build()
        .map_err(|e| AppError::Other(format!("WebView açılamadı: {e}")))?;

    {
        let state = app.state::<AppState>();
        let value = CurrentSite {
            site_id: site.id.clone(),
            host,
            window_label: label.clone(),
        };
        *state.current_site.lock().expect("current_site lock") = Some(value);
    }

    // Keep a handle alive so the window is not dropped mid-build.
    let _ = window;
    Ok(label)
}

// ---------------------------------------------------------------------------
// Mobile: a single WebView, isolated by swapping the cookie jar.
// ---------------------------------------------------------------------------

fn open_on_mobile(app: &AppHandle, site: &Site, url: &Url, init_script: &str) -> AppResult<String> {
    let window = app
        .get_webview_window(LAUNCHER_LABEL)
        .ok_or_else(|| AppError::Other("ana WebView bulunamadı".into()))?;

    let state = app.state::<AppState>();

    // 1. Export the outgoing site's cookies before anything is cleared.
    if let Some(current) = state
        .current_site
        .lock()
        .expect("current_site lock")
        .clone()
    {
        export_cookies(&window, &current.site_id)?;
    }

    // 2. Clear the shared jar, then load the target site's cookies.
    window
        .clear_all_browsing_data()
        .map_err(|e| AppError::Other(format!("çerezler temizlenemedi: {e}")))?;
    import_cookies(&window, &site.id)?;

    // 3. Install the cosmetic/anti-popup script for this navigation.
    window
        .eval(init_script)
        .map_err(|e| AppError::Other(format!("script yüklenemedi: {e}")))?;

    window
        .navigate(url.clone())
        .map_err(|e| AppError::Other(format!("sayfa açılamadı: {e}")))?;

    *state.current_site.lock().expect("current_site lock") = Some(CurrentSite {
        site_id: site.id.clone(),
        host: site.host(),
        window_label: LAUNCHER_LABEL.into(),
    });

    Ok(LAUNCHER_LABEL.to_string())
}

/// Save the WebView's current cookie jar into the named site's encrypted blob.
fn export_cookies(window: &tauri::WebviewWindow, site_id: &str) -> AppResult<()> {
    let app = window.app_handle().clone();
    let state = app.state::<AppState>();

    let live = window
        .cookies()
        .map_err(|e| AppError::Other(e.to_string()))?;
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

/// Close a site session, persisting its cookies first.
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

    if cfg!(any(target_os = "android", target_os = "ios")) {
        // Mobile: save the jar, then send the WebView back to the launcher.
        if let Some(current) = &current {
            if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
                let _ = export_cookies(&window, &current.site_id);
            }
        }
        if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
            let state = app.state::<AppState>();
            let back = state
                .launcher_url
                .lock()
                .expect("launcher_url lock")
                .clone();
            if let Some(url) = back {
                let _ = window.navigate(url);
            }
        }
        let state = app.state::<AppState>();
        *state.current_site.lock().expect("current_site lock") = None;
        return Ok(());
    }

    if let Some(window) = app.get_webview_window(label) {
        if let Some(current) = &current {
            let _ = export_cookies(&window, &current.site_id);
        }
        window.close().map_err(|e| AppError::Other(e.to_string()))?;
    }

    // Only clear the tracking entry if it was this window's site.
    let matches_current = current.as_ref().is_some_and(|c| c.window_label == label);
    if matches_current {
        let state = app.state::<AppState>();
        *state.current_site.lock().expect("current_site lock") = None;
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
    tauri_plugin_opener::open_url(safe.as_str(), None::<&str>)
        .map_err(|e| AppError::Other(e.to_string()))
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
