//! Android-specific bridges.
//!
//! All are implemented in Kotlin (see `src-tauri/android-plugin/`) because the
//! platform APIs involved — `AndroidKeyStore`, `PictureInPictureParams`, the
//! androidx.webkit multi-profile store and the site WebViews themselves — are
//! only reachable from the JVM side.
//!
//! The `site_*` functions drive the per-site WebView profiles: each registered
//! site opens in its own WebView bound to its own persistent profile, and the
//! launcher's shared WebView (default profile) is never used for site data.

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

// ---------------------------------------------------------------------------
// Per-site WebView profile bridges.
// ---------------------------------------------------------------------------

/// User-facing guidance when the WebView provider lacks multi-profile
/// support. Deliberately explicit: the site is refused rather than opened on
/// the shared default profile, because "open but not isolated" would silently
/// reintroduce the cross-site data mixing this feature removes.
pub const MULTI_PROFILE_UNSUPPORTED_MSG: &str = "Bu cihazın WebView sağlayıcısı çoklu profil (MULTI_PROFILE) desteği sunmuyor; site verileri izole edilemediği için site açılamadı. Android System WebView veya Google Chrome'u güncelleyin; güncelleme sonrası da devam ederse bu cihaz bu özelliği desteklemiyor olabilir.";

/// User-facing explanation when Kotlin refuses a *quarantined* profile: its
/// last in-place wipe timed out on `removeAllCookies`, a destructive provider
/// call that offers no cancellation, so its completion is unverified and it
/// may still delete cookies belonging to a session that reopens the same
/// profile. The recovery step is part of the message: this is a retryable
/// state, not a dead end.
pub const PROFILE_QUARANTINED_MSG: &str = "Bu sitenin verileri temizlenirken çerez silme işlemi zaman aşımına uğradı ve tamamlanıp tamamlanmadığı doğrulanamadı. İşlem arka planda hâlâ sürüyor olabileceği için site, temizlik doğrulanana kadar açılmadı: açılsaydı yeni oturumun çerezleri silinebilirdi. Site verilerini yeniden temizlemeyi deneyin; sonuç değişmezse siteyi kaldırıp yeniden ekleyin. Kayıtlı oturum verileriniz korunuyor.";

/// Map a Kotlin refusal from the two-phase open onto a user-facing error.
///
/// Host-compiled (and host-tested) on the plugin crate's error type alone —
/// deciding what the user reads needs no Android runtime:
///
///  * `unsupported-multi-profile` → [`AppError::Unsupported`] carrying
///    [`MULTI_PROFILE_UNSUPPORTED_MSG`]: a feature gap, never retried on the
///    shared default profile;
///  * `profile-quarantined` → [`AppError::Quarantined`] carrying
///    [`PROFILE_QUARANTINED_MSG`]: the sentinel is an internal contract
///    between Kotlin and Rust, so the user gets the explanation and the
///    recovery step instead of the raw token (and not `Other`'s "unexpected
///    error" wording either — this refusal is designed, transient and
///    retryable);
///  * anything else → the bridge message unchanged (the frontend renders
///    `to_string()` directly).
///
/// A quarantine is deliberately NOT routed through the `migration-partial`
/// retirement path: the profile is the user's live one, and the answer is to
/// verify the wipe — not to delete the name and re-import the legacy jar.
pub fn classify_site_open_error(error: animehub_android::Error) -> AppError {
    if animehub_android::is_unsupported_multi_profile(&error) {
        AppError::Unsupported(MULTI_PROFILE_UNSUPPORTED_MSG.to_string())
    } else if animehub_android::is_profile_quarantined(&error) {
        AppError::Quarantined(PROFILE_QUARANTINED_MSG.to_string())
    } else {
        AppError::Other(error.to_string())
    }
}

/// What happened to a profile deletion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteProfileAction {
    /// The provider accepted the deletion (disk work may still finish).
    Deleted,
    /// No such profile — already clean.
    Absent,
    /// Deletion could not run right now because the profile is loaded in
    /// this process. The caller queues the name and retries at the next
    /// process start. A provider WITHOUT profile support no longer lands
    /// here — Kotlin reports `unsupported-multi-profile` as an error and the
    /// bridge classifies it as [`AppError::Unsupported`], a feature gap that
    /// must never be presented as "deferred".
    Pending,
}

/// Phase 1 of the two-phase open: import the one-time legacy 0.3.4 cookie
/// jar into `profile` WITHOUT creating the site WebView.
///
/// Kotlin reports success only when every `setCookie` callback accepted and
/// the profile flush completed (see `SiteMigrationFlow`), so a success here
/// is the strict "migration landed" criterion the caller persists
/// `profilemig-<site>` against. An incomplete import arrives as
/// [`AppError::Other`] carrying the `migration-partial` sentinel — the
/// caller retires that profile name and retries on a fresh generation.
/// Fails with [`AppError::Unsupported`] carrying
/// [`MULTI_PROFILE_UNSUPPORTED_MSG`] when the provider cannot isolate sites,
/// and with [`PROFILE_QUARANTINED_MSG`] when the target profile's last
/// in-place wipe is still unverified — in that case no cookie is written into
/// the profile and the caller never persists its migration flag.
#[cfg(target_os = "android")]
pub fn site_migrate(url: &str, profile: &str, cookies: &[String]) -> AppResult<()> {
    animehub_android::site_migrate(url.to_string(), profile.to_string(), cookies.to_vec())
        .map_err(classify_site_open_error)
}

/// Phase 2: open `url` in a new site WebView bound to `profile`.
///
/// Carries no cookies: the jar import ran through [`site_migrate`] and its
/// flag is durable before this call, so a view the user sees can never exist
/// while the migration state is unrecorded. Fails with
/// [`AppError::Unsupported`] carrying [`MULTI_PROFILE_UNSUPPORTED_MSG`] when
/// the provider cannot isolate sites, and with [`PROFILE_QUARANTINED_MSG`]
/// when Kotlin refuses to bind a WebView to a profile whose last wipe timed
/// out on a provider call it cannot cancel. Both refusals build nothing, so
/// the caller claims no session and the refusal is simply retryable.
#[cfg(target_os = "android")]
pub fn site_open(url: &str, profile: &str, init_script: &str) -> AppResult<()> {
    animehub_android::site_open(
        url.to_string(),
        profile.to_string(),
        init_script.to_string(),
    )
    .map_err(classify_site_open_error)
}

/// Close the active site WebView (flushing its profile cookies unless
/// `flush=false`). Idempotent on the Kotlin side.
#[cfg(target_os = "android")]
pub fn site_close(flush: bool) -> AppResult<()> {
    animehub_android::site_close(flush).map_err(|e| AppError::Other(e.to_string()))?;
    Ok(())
}

/// Delete one site profile's data — never the default profile: Kotlin
/// validates the name against `android_profile_name`'s exact shape.
///
/// `unsupported-multi-profile` classifies to [`AppError::Unsupported`] so
/// callers can distinguish "feature gap" from "try again later".
#[cfg(target_os = "android")]
pub fn site_profile_delete(profile: &str) -> AppResult<SiteProfileAction> {
    match animehub_android::site_profile_delete(profile.to_string()) {
        Ok(r) => {
            if r.deleted {
                Ok(SiteProfileAction::Deleted)
            } else if r.pending {
                Ok(SiteProfileAction::Pending)
            } else {
                Ok(SiteProfileAction::Absent)
            }
        }
        Err(e) if animehub_android::is_unsupported_multi_profile(&e) => Err(AppError::Unsupported(
            MULTI_PROFILE_UNSUPPORTED_MSG.to_string(),
        )),
        Err(e) => Err(AppError::Other(e.to_string())),
    }
}

/// Clear one site profile in place (cookies, web storage, geolocation
/// permissions, storage, final flush).
///
/// The Kotlin flow only reports success when every required step completed
/// (see `SiteClearFlow`); any failure — including the provider being unable
/// to address profiles at all — propagates here as an error. `clear_site_data`
/// therefore aborts BEFORE `delete_site_session`, so Rust's encrypted session
/// blobs stay intact for a retry and a half-finished wipe is never surfaced
/// as a false success. An `unsupported-multi-profile` result classifies to
/// [`AppError::Unsupported`] with the user-facing MULTI_PROFILE guidance.
#[cfg(target_os = "android")]
pub fn site_clear_data(profile: &str) -> AppResult<()> {
    match animehub_android::site_clear_data(profile.to_string()) {
        Ok(_) => Ok(()),
        Err(e) if animehub_android::is_unsupported_multi_profile(&e) => Err(AppError::Unsupported(
            MULTI_PROFILE_UNSUPPORTED_MSG.to_string(),
        )),
        Err(e) => Err(AppError::Other(e.to_string())),
    }
}

/// Flush the active site profile's cookie jar (periodic durability sync).
#[cfg(target_os = "android")]
pub fn site_flush() -> AppResult<()> {
    animehub_android::site_flush().map_err(|e| AppError::Other(e.to_string()))?;
    Ok(())
}

/// Every profile name in the provider store (unfiltered — callers restrict
/// themselves to [`crate::web::windows::ANDROID_PROFILE_PREFIX`]).
///
/// An unsupported provider returns an empty list with a warning: there is
/// nothing this app can enumerate or delete through the API there, which the
/// docs record as the provider-downgrade edge case.
#[cfg(target_os = "android")]
pub fn site_profile_names() -> AppResult<Vec<String>> {
    let r = animehub_android::site_profile_names().map_err(|e| AppError::Other(e.to_string()))?;
    if r.unsupported {
        log::warn!("profil listesi alınamadı: sağlayıcı çoklu profili desteklemiyor");
    }
    Ok(r.names)
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
pub fn site_migrate(_url: &str, _profile: &str, _cookies: &[String]) -> AppResult<()> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn site_open(_url: &str, _profile: &str, _init_script: &str) -> AppResult<()> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn site_close(_flush: bool) -> AppResult<()> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn site_profile_delete(_profile: &str) -> AppResult<SiteProfileAction> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn site_clear_data(_profile: &str) -> AppResult<()> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn site_flush() -> AppResult<()> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
    ))
}

#[cfg(not(target_os = "android"))]
pub fn site_profile_names() -> AppResult<Vec<String>> {
    Err(AppError::Unsupported(
        "Site profilleri yalnızca Android'de kullanılabilir".into(),
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

    #[test]
    fn open_refusals_are_classified_into_user_facing_errors() {
        // The mapping is host-compiled, so every platform proves what the user
        // reads when Kotlin refuses an open — including the quarantine, whose
        // whole point is that a raw sentinel must never reach the UI.
        let unsupported = classify_site_open_error(animehub_android::Error::Bridge(
            animehub_android::MULTI_PROFILE_SENTINEL.to_string(),
        ));
        assert_eq!(unsupported.code(), "unsupported");
        assert_eq!(unsupported.to_string(), MULTI_PROFILE_UNSUPPORTED_MSG);

        let quarantined = classify_site_open_error(animehub_android::Error::Bridge(format!(
            "{}: {}",
            animehub_android::QUARANTINE_SENTINEL,
            "çerez temizliği zaman aşımına uğradı, işlem doğrulanamadı"
        )));
        assert_eq!(
            quarantined.code(),
            "quarantined",
            "a designed refusal is neither 'other' nor 'unsupported'"
        );
        assert_eq!(quarantined.to_string(), PROFILE_QUARANTINED_MSG);
        assert!(
            !quarantined
                .to_string()
                .contains(animehub_android::QUARANTINE_SENTINEL),
            "the internal sentinel must not be shown to the user"
        );
        assert!(
            quarantined.to_string().contains("yeniden temizlemeyi"),
            "the message must name the recovery step"
        );

        // Anything else passes through unchanged (the bridge's own prefix
        // included), so no Kotlin detail is lost for unclassified failures.
        let other = classify_site_open_error(animehub_android::Error::Bridge(
            "site açılamadı: patladı".to_string(),
        ));
        assert_eq!(other.code(), "other");
        assert!(other.to_string().contains("patladı"), "{other}");

        // A migration retry is NOT a quarantine: it must keep its own meaning
        // (retire the profile name and re-import), so it stays unclassified
        // here and is handled by the open protocol instead.
        let retry = classify_site_open_error(animehub_android::Error::Bridge(format!(
            "{}: profil zaten mevcut",
            animehub_android::MIGRATION_RETRY_SENTINEL
        )));
        assert_eq!(retry.code(), "other");
        assert!(retry.to_string().contains("migration-partial"), "{retry}");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_stubs_report_unsupported() {
        assert!(!enter_pip(16, 9).unwrap());
        assert!(!set_pip_auto_enter(true).unwrap());
        assert_eq!(keystore_seal("p", b"x").unwrap_err().code(), "keyring");
        assert_eq!(keystore_open("p", "x").unwrap_err().code(), "keyring");
        // Site-profile bridges must fail honestly off Android — the removed
        // JSON storage/cookie transfer commands are gone from both sides.
        // Both phases of the two-phase open refuse on their own.
        let profile = "site-00000000000000000000000000000000-g0";
        assert_eq!(
            site_migrate("https://example.com/", profile, &[])
                .unwrap_err()
                .code(),
            "unsupported"
        );
        assert_eq!(
            site_open("https://example.com/", profile, "")
                .unwrap_err()
                .code(),
            "unsupported"
        );
        assert_eq!(site_close(true).unwrap_err().code(), "unsupported");
        assert_eq!(
            site_profile_delete(profile).unwrap_err().code(),
            "unsupported"
        );
        assert_eq!(site_clear_data(profile).unwrap_err().code(), "unsupported");
        assert_eq!(site_flush().unwrap_err().code(), "unsupported");
        assert_eq!(site_profile_names().unwrap_err().code(), "unsupported");
    }

    #[test]
    fn removed_storage_transfer_fns_are_gone() {
        // Compile-level guard: the old localStorage/IndexedDB/cookie-swap
        // entry points no longer exist anywhere in this module. Names are
        // assembled so this test does not match its own literals.
        let src = include_str!("android_bridge.rs");
        for name in [
            "localstorage_export",
            "localstorage_import",
            "indexeddb_export",
            "indexeddb_import",
            "webview_cookies_get",
            "webview_cookies_replace",
            "webview_cookies_clear",
        ] {
            let gone = format!("pub fn {name}");
            assert!(
                !src.contains(&gone),
                "legacy storage-transfer bridge still present: {gone}"
            );
        }
    }
}
