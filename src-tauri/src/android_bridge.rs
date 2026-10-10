//! Android-specific bridges.
//!
//! All are implemented in Kotlin (see `src-tauri/android-plugin/`) because the
//! platform APIs involved — `AndroidKeyStore`, `PictureInPictureParams`,
//! `CookieManager`, and `evaluateJavascript` — are only reachable from the JVM
//! side.

use crate::error::{AppError, AppResult};

/// Encrypt `plaintext` with the Android Keystore-backed key.
///
/// The raw key never leaves the secure hardware / TEE; Rust only ever sees
/// ciphertext. `purpose` is bound into the alias so two stores cannot swap
/// blobs. The plugin crate returns its own `KeystoreResult` — read `.value`
/// straight off it rather than restating the type under the same name.
#[cfg(target_os = "android")]
pub fn keystore_seal(purpose: &str, plaintext: &[u8]) -> AppResult<String> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(plaintext);
    let r = animehub_android::keystore_seal(purpose.into(), b64)
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    Ok(r.value)
}

/// Decrypt a blob produced by [`keystore_seal`].
#[cfg(target_os = "android")]
pub fn keystore_open(purpose: &str, sealed: &str) -> AppResult<Vec<u8>> {
    use base64::Engine as _;
    let r = animehub_android::keystore_open(purpose.into(), sealed.to_string())
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    base64::engine::general_purpose::STANDARD
        .decode(r.value)
        .map_err(|_| AppError::Crypto("base64 çözülemedi".into()))
}

/// Export the live WebView's `localStorage`, Keystore-sealed, as base64.
///
/// Blocks until the page JS ran and the blob was sealed, so the export is
/// complete on return. Must be called while the site page is still loaded.
#[cfg(target_os = "android")]
pub fn localstorage_export(purpose: &str) -> AppResult<String> {
    let r = animehub_android::localstorage_export(purpose.into())
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    Ok(r.value)
}

/// Restore a [`localstorage_export`] blob into the currently loaded page.
#[cfg(target_os = "android")]
pub fn localstorage_import(purpose: &str, sealed: &str) -> AppResult<()> {
    let r = animehub_android::localstorage_import(purpose.into(), sealed.to_string())
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    if !r.ok {
        return Err(AppError::Storage("localStorage geri yüklenemedi".into()));
    }
    Ok(())
}

/// Export the live WebView's IndexedDB databases (best effort), sealed.
#[cfg(target_os = "android")]
pub fn indexeddb_export(purpose: &str) -> AppResult<String> {
    let r = animehub_android::indexeddb_export(purpose.into())
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    Ok(r.value)
}

/// Restore an [`indexeddb_export`] blob into the currently loaded page.
#[cfg(target_os = "android")]
pub fn indexeddb_import(purpose: &str, sealed: &str) -> AppResult<()> {
    let r = animehub_android::indexeddb_import(purpose.into(), sealed.to_string())
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    if !r.ok {
        return Err(AppError::Storage("IndexedDB geri yüklenemedi".into()));
    }
    Ok(())
}

/// Read the live Android CookieManager header for a validated HTTP(S) URL.
/// The Kotlin side normalizes Android's expected `null` (no cookies) to `""`.
#[cfg(target_os = "android")]
pub fn webview_cookies_get(url: &str) -> AppResult<String> {
    let r = animehub_android::webview_cookies_get(url.to_string())
        .map_err(|e| AppError::Other(e.to_string()))?;
    Ok(r.value)
}

/// Clear the shared Android CookieManager and restore the supplied provider
/// jar. Kotlin resolves after the asynchronous CookieManager callbacks finish.
#[cfg(target_os = "android")]
pub fn webview_cookies_replace(url: &str, cookies: &[String]) -> AppResult<()> {
    let r = animehub_android::webview_cookies_replace(url.to_string(), cookies.to_vec())
        .map_err(|e| AppError::Other(e.to_string()))?;
    if !r.ok {
        return Err(AppError::Storage(
            "Android çerezleri geri yüklenemedi".into(),
        ));
    }
    Ok(())
}

/// Clear Android's live shared cookie jar without changing provider snapshots.
#[cfg(target_os = "android")]
pub fn webview_cookies_clear() -> AppResult<()> {
    webview_cookies_replace("", &[])
}

// ---------------------------------------------------------------------------
// Desktop stubs: same signatures, honest failures, so call sites compile and
// stay uniform on every platform.
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "android"))]
pub fn keystore_seal(_purpose: &str, _plaintext: &[u8]) -> AppResult<String> {
    Err(AppError::Keyring(
        "Keystore yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn keystore_open(_purpose: &str, _sealed: &str) -> AppResult<Vec<u8>> {
    Err(AppError::Keyring(
        "Keystore yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn localstorage_export(_purpose: &str) -> AppResult<String> {
    Err(AppError::Keyring(
        "Depolama aktarımı yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn localstorage_import(_purpose: &str, _sealed: &str) -> AppResult<()> {
    Err(AppError::Keyring(
        "Depolama aktarımı yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn indexeddb_export(_purpose: &str) -> AppResult<String> {
    Err(AppError::Keyring(
        "Depolama aktarımı yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn indexeddb_import(_purpose: &str, _sealed: &str) -> AppResult<()> {
    Err(AppError::Keyring(
        "Depolama aktarımı yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn webview_cookies_get(_url: &str) -> AppResult<String> {
    Err(AppError::Other(
        "CookieManager yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn webview_cookies_replace(_url: &str, _cookies: &[String]) -> AppResult<()> {
    Err(AppError::Other(
        "CookieManager yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn webview_cookies_clear() -> AppResult<()> {
    Err(AppError::Other(
        "CookieManager yalnızca Android'de kullanılabilir".into(),
    ))
}

/// Enter Picture-in-Picture mode.
///
/// On Android the Kotlin side runs the controlled chain: it prepares the
/// WebView with the site's `window.__animehubPreparePip()` (player-only view,
/// detected aspect ratio, player rect as `sourceRectHint`), waits for the JS
/// answer and only then calls `enterPictureInPictureMode()`. The `num`/`den`
/// passed here are the fallback ratio, already clamped by `clamp_aspect`.
///
/// On desktop there is no system PiP for an arbitrary WebView, so the command
/// reports unsupported rather than faking a floating window.
#[cfg(target_os = "android")]
pub fn enter_pip(aspect_num: u32, aspect_den: u32) -> AppResult<bool> {
    animehub_android::enter_pip(aspect_num, aspect_den).map_err(|e| AppError::Other(e.to_string()))
}

#[cfg(not(target_os = "android"))]
pub fn enter_pip(_aspect_num: u32, _aspect_den: u32) -> AppResult<bool> {
    Ok(false)
}

/// Allow or deny the *controlled* PiP paths.
///
/// On Android the Kotlin side stores the preference and uses it for
/// `onPictureInPictureRequested()` (API 30+) and `onUserLeaveHint()`
/// (API 26-29). The platform's own `setAutoEnterEnabled` stays `false`: AOSP
/// documents that it suppresses `onPictureInPictureRequested()`, so the system
/// would enter PiP before the WebView could be prepared for it.
///
/// Returns `false` on devices without PiP support (and on desktop, where the
/// whole idea does not apply).
#[cfg(target_os = "android")]
pub fn set_pip_auto_enter(enabled: bool) -> AppResult<bool> {
    animehub_android::set_pip_auto_enter(enabled).map_err(|e| AppError::Other(e.to_string()))
}

#[cfg(not(target_os = "android"))]
pub fn set_pip_auto_enter(_enabled: bool) -> AppResult<bool> {
    Ok(false)
}

/// The PiP diagnostic lines for the developer panel. Empty off Android.
#[cfg(target_os = "android")]
pub fn debug_log() -> AppResult<Vec<String>> {
    animehub_android::debug_log().map_err(|e| AppError::Other(e.to_string()))
}

#[cfg(not(target_os = "android"))]
pub fn debug_log() -> AppResult<Vec<String>> {
    Ok(Vec::new())
}

/// Clamp a video aspect ratio into the range Android accepts for PiP.
///
/// The platform rejects ratios outside roughly 1:2.39 .. 2.39:1, and a
/// rejected ratio silently disables PiP, so we clamp defensively.
pub fn clamp_aspect(num: u32, den: u32) -> (u32, u32) {
    if den == 0 || num == 0 {
        return (16, 9);
    }
    let ratio = num as f64 / den as f64;
    const MIN: f64 = 1.0 / 2.39;
    const MAX: f64 = 2.39;
    if ratio < MIN {
        (100, 239)
    } else if ratio > MAX {
        (239, 100)
    } else {
        (num, den)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_leaves_normal_video_ratios_alone() {
        assert_eq!(clamp_aspect(16, 9), (16, 9));
        assert_eq!(clamp_aspect(4, 3), (4, 3));
        assert_eq!(clamp_aspect(21, 9), (21, 9));
    }

    #[test]
    fn clamp_rejects_degenerate_input() {
        assert_eq!(clamp_aspect(0, 0), (16, 9));
        assert_eq!(clamp_aspect(16, 0), (16, 9));
        assert_eq!(clamp_aspect(0, 9), (16, 9));
    }

    #[test]
    fn clamp_bounds_extreme_ratios() {
        assert_eq!(clamp_aspect(1, 100), (100, 239), "too tall");
        assert_eq!(clamp_aspect(100, 1), (239, 100), "too wide");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_stubs_report_unsupported() {
        assert!(!enter_pip(16, 9).unwrap());
        assert!(!set_pip_auto_enter(true).unwrap());
        assert_eq!(keystore_seal("p", b"x").unwrap_err().code(), "keyring");
        assert_eq!(keystore_open("p", "x").unwrap_err().code(), "keyring");
        assert_eq!(localstorage_export("p").unwrap_err().code(), "keyring");
        assert_eq!(localstorage_import("p", "x").unwrap_err().code(), "keyring");
        assert_eq!(indexeddb_export("p").unwrap_err().code(), "keyring");
        assert_eq!(indexeddb_import("p", "x").unwrap_err().code(), "keyring");
        assert_eq!(
            webview_cookies_get("https://example.com")
                .unwrap_err()
                .code(),
            "other"
        );
        assert_eq!(
            webview_cookies_replace("https://example.com", &[])
                .unwrap_err()
                .code(),
            "other"
        );
        assert_eq!(webview_cookies_clear().unwrap_err().code(), "other");
    }
}
