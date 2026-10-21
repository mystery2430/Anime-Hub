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
        .map_err(|_| AppError::Keyring("Android Keystore operation failed".into()))?;
    Ok(r.value)
}

/// Decrypt a blob produced by [`keystore_seal`].
#[cfg(target_os = "android")]
pub fn keystore_open(purpose: &str, sealed: &str) -> AppResult<Vec<u8>> {
    use base64::Engine as _;
    let r = animehub_android::keystore_open(purpose.into(), sealed.to_string())
        .map_err(|_| AppError::Keyring("Android Keystore operation failed".into()))?;
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
pub const MULTI_PROFILE_UNSUPPORTED_MSG: &str = "Bu cihazın WebView sağlayıcısı çoklu profil (MULTI_PROFILE) desteği sunmuyor; site verileri izole edilemediği için site açılamadı. AnimeHub Android 10 (API 29) ve üzeri ile güncel bir Android System WebView gerektirir. WebView veya Google Chrome'u güncelleyin; güncelleme sonrası da devam ederse bu cihaz bu özelliği desteklemiyor olabilir.";

/// User-facing explanation when Kotlin refuses a *quarantined* profile: an
/// in-place wipe timed out on `removeAllCookies`, a destructive provider call
/// that offers no cancellation, so its completion is unverified and it may
/// still delete cookies belonging to a session that reopens the same profile.
/// Kotlin tracks each such deletion as its OWN outstanding operation, so the
/// refusal can outlive the attempt that reported it — a later successful clear
/// does not reopen the site while an earlier deletion is still in flight, and
/// the message says so. The recovery step is part of it: this is a retryable
/// state, not a dead end.
pub const PROFILE_QUARANTINED_MSG: &str = "Bu sitenin verileri temizlenirken çerez silme işlemi zaman aşımına uğradı ve tamamlanıp tamamlanmadığı doğrulanamadı. İşlem arka planda hâlâ sürüyor olabileceği için site, bekleyen tüm silme işlemleri sonuçlanıp temizlik doğrulanana kadar açılmadı: açılsaydı yeni oturumun çerezleri silinebilirdi. Sonradan yapılan başarılı bir temizlik, daha önce zaman aşımına uğrayan silme işlemini beklemekten vazgeçirmez. Site verilerini yeniden temizlemeyi deneyin; sonuç değişmezse siteyi kaldırıp yeniden ekleyin. Kayıtlı oturum verileriniz korunuyor.";

/// Map Kotlin's closed protocol sentinels to actionable errors without ever
/// forwarding provider exception text, profile names or other bridge payloads.
pub fn classify_site_open_error(error: animehub_android::Error) -> AppError {
    if animehub_android::is_unsupported_multi_profile(&error) {
        AppError::Unsupported(MULTI_PROFILE_UNSUPPORTED_MSG.to_string())
    } else if animehub_android::is_profile_quarantined(&error) {
        AppError::Quarantined(PROFILE_QUARANTINED_MSG.to_string())
    } else if animehub_android::is_migration_retry(&error) {
        // The protocol checks this sentinel to retire the incomplete virgin
        // profile and retry exactly once. Keep the code, discard the provider
        // text: it can contain cookie/header details.
        AppError::Other(format!(
            "{}: site cookie migration did not complete",
            animehub_android::MIGRATION_RETRY_SENTINEL
        ))
    } else {
        AppError::Other("Android site profile operation failed".into())
    }
}

#[cfg(target_os = "android")]
fn plugin_error_class(error: &animehub_android::Error) -> &'static str {
    if animehub_android::is_unsupported_multi_profile(error) {
        "unsupported"
    } else if animehub_android::is_profile_quarantined(error) {
        "quarantined"
    } else if animehub_android::is_migration_retry(error) {
        "migration_partial"
    } else {
        match error {
            animehub_android::Error::Bridge(_) => "ipc",
            animehub_android::Error::Unsupported => "unsupported",
            animehub_android::Error::Keystore(_) => "storage",
            animehub_android::Error::Pip(_) => "provider",
        }
    }
}

#[cfg(target_os = "android")]
fn record_bridge_start(operation_id: &crate::diagnostics::OperationId, area: &str, phase: &str) {
    crate::diagnostics::record_for_operation(
        Some(operation_id),
        area,
        phase,
        "started",
        None,
        None,
        None,
        None,
        None,
    );
}

#[cfg(target_os = "android")]
fn record_bridge_result(
    operation_id: &crate::diagnostics::OperationId,
    area: &str,
    phase: &str,
    started: std::time::Instant,
    result: &str,
    error_class: Option<&str>,
) {
    crate::diagnostics::record_for_operation(
        Some(operation_id),
        area,
        phase,
        result,
        Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64),
        None,
        error_class,
        None,
        None,
    );
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
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "migration");
    let started = std::time::Instant::now();
    match animehub_android::site_migrate(
        operation_id.as_str().to_owned(),
        url.to_string(),
        profile.to_string(),
        cookies.to_vec(),
    ) {
        Ok(_) => {
            record_bridge_result(&operation_id, "bridge", "migration", started, "ok", None);
            Ok(())
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            let result = if matches!(class, "unsupported" | "quarantined") {
                class
            } else {
                "failed"
            };
            record_bridge_result(
                &operation_id,
                "bridge",
                "migration",
                started,
                result,
                Some(class),
            );
            Err(classify_site_open_error(error))
        }
    }
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
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "open");
    let started = std::time::Instant::now();
    match animehub_android::site_open(
        operation_id.as_str().to_owned(),
        url.to_string(),
        profile.to_string(),
        init_script.to_string(),
    ) {
        Ok(_) => {
            record_bridge_result(&operation_id, "bridge", "open", started, "ok", None);
            Ok(())
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            let result = if matches!(class, "unsupported" | "quarantined") {
                class
            } else {
                "failed"
            };
            record_bridge_result(
                &operation_id,
                "bridge",
                "open",
                started,
                result,
                Some(class),
            );
            Err(classify_site_open_error(error))
        }
    }
}

/// Close the active site WebView (flushing its profile cookies unless
/// `flush=false`). Idempotent on the Kotlin side.
#[cfg(target_os = "android")]
pub fn site_close(flush: bool) -> AppResult<()> {
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "close");
    let started = std::time::Instant::now();
    match animehub_android::site_close(operation_id.as_str().to_owned(), flush) {
        Ok(_) => {
            record_bridge_result(&operation_id, "bridge", "close", started, "ok", None);
            Ok(())
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "close",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other("site close did not complete".into()))
        }
    }
}

/// Delete one site profile's data — never the default profile: Kotlin
/// validates the name against `android_profile_name`'s exact shape.
///
/// `unsupported-multi-profile` classifies to [`AppError::Unsupported`] so
/// callers can distinguish "feature gap" from "try again later".
#[cfg(target_os = "android")]
pub fn site_profile_delete(profile: &str) -> AppResult<SiteProfileAction> {
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "profile_delete");
    let started = std::time::Instant::now();
    match animehub_android::site_profile_delete(
        operation_id.as_str().to_owned(),
        profile.to_string(),
    ) {
        Ok(reply) => {
            let (action, result) = if reply.deleted {
                (SiteProfileAction::Deleted, "deleted")
            } else if reply.pending {
                (SiteProfileAction::Pending, "deferred")
            } else {
                (SiteProfileAction::Absent, "absent")
            };
            record_bridge_result(
                &operation_id,
                "bridge",
                "profile_delete",
                started,
                result,
                None,
            );
            Ok(action)
        }
        Err(error) if animehub_android::is_unsupported_multi_profile(&error) => {
            record_bridge_result(
                &operation_id,
                "bridge",
                "profile_delete",
                started,
                "unsupported",
                Some("unsupported"),
            );
            Err(AppError::Unsupported(
                MULTI_PROFILE_UNSUPPORTED_MSG.to_string(),
            ))
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "profile_delete",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other("site profile deletion failed".into()))
        }
    }
}

/// Retire one profile name's quarantine ledger; returns how many outstanding
/// destructive-operation records were dropped with it.
///
/// ORDER IS THE CALLER'S SAFETY PROPERTY: invoke this only after the site's
/// generation bump is durably stored — [`crate::web::windows::run_mobile_delete_protocol`]
/// does exactly that. Kotlin no longer retires when the provider merely
/// *accepts* a deletion, because the bump (or its storage write) can still fail;
/// retiring first would leave the same name both derivable again and unblocked,
/// with a `removeAllCookies` deletion possibly still in flight on it.
#[cfg(target_os = "android")]
pub fn site_profile_retire(profile: &str) -> AppResult<u32> {
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "retire");
    let started = std::time::Instant::now();
    match animehub_android::site_profile_retire(
        operation_id.as_str().to_owned(),
        profile.to_string(),
    ) {
        Ok(reply) => {
            record_bridge_result(&operation_id, "bridge", "retire", started, "retired", None);
            Ok(reply.dropped)
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "retire",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other(
                "profile quarantine retirement failed".into(),
            ))
        }
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
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "clear");
    let started = std::time::Instant::now();
    match animehub_android::site_clear_data(operation_id.as_str().to_owned(), profile.to_string()) {
        Ok(_) => {
            record_bridge_result(&operation_id, "bridge", "clear", started, "ok", None);
            Ok(())
        }
        Err(error) if animehub_android::is_unsupported_multi_profile(&error) => {
            record_bridge_result(
                &operation_id,
                "bridge",
                "clear",
                started,
                "unsupported",
                Some("unsupported"),
            );
            Err(AppError::Unsupported(
                MULTI_PROFILE_UNSUPPORTED_MSG.to_string(),
            ))
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            let result = if class == "quarantined" {
                "quarantined"
            } else {
                "failed"
            };
            record_bridge_result(
                &operation_id,
                "bridge",
                "clear",
                started,
                result,
                Some(class),
            );
            Err(AppError::Other(
                "site data clear failed; encrypted session data was retained".into(),
            ))
        }
    }
}

/// Flush the active site profile's cookie jar (periodic durability sync).
#[cfg(target_os = "android")]
pub fn site_flush() -> AppResult<()> {
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    let started = std::time::Instant::now();
    match animehub_android::site_flush(operation_id.as_str().to_owned()) {
        Ok(_) => Ok(()),
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "flush",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other("site profile flush failed".into()))
        }
    }
}

/// Replace the blocklist snapshot evaluated by the Android site WebView.
///
/// Best-effort but logged: a failed push means the built-in host/IP rules still
/// apply, while the user blocklist would silently regress to desktop-only.
/// Callers must never open a site before this returns.
pub fn site_blocklist_update(entries: &[String]) -> AppResult<()> {
    #[cfg(target_os = "android")]
    {
        let operation_id = crate::diagnostics::current_or_new_operation_id();
        animehub_android::site_blocklist_update(operation_id.as_str().to_owned(), entries.to_vec())
            .map_err(|_| AppError::Other("Android blocklist could not be updated".into()))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = entries;
        Ok(())
    }
}

/// Every profile name in the provider store (unfiltered — callers restrict
/// themselves to [`crate::web::windows::ANDROID_PROFILE_PREFIX`]).
///
/// An unsupported provider returns an empty list with a warning: there is
/// nothing this app can enumerate or delete through the API there, which the
/// docs record as the provider-downgrade edge case.
#[cfg(target_os = "android")]
pub fn site_profile_names() -> AppResult<Vec<String>> {
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "profile_list");
    let started = std::time::Instant::now();
    match animehub_android::site_profile_names(operation_id.as_str().to_owned()) {
        Ok(reply) => {
            if reply.unsupported {
                record_bridge_result(
                    &operation_id,
                    "bridge",
                    "capability",
                    started,
                    "unsupported_provider",
                    Some("unsupported"),
                );
                return Ok(Vec::new());
            }
            let count = reply.names.len() as u64;
            record_bridge_result(&operation_id, "bridge", "profile_list", started, "ok", None);
            crate::diagnostics::record_for_operation(
                Some(&operation_id),
                "profile",
                "profile_list",
                "ok",
                None,
                None,
                None,
                Some(count),
                None,
            );
            Ok(reply.names)
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "profile_list",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other("site profile enumeration failed".into()))
        }
    }
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
pub fn site_profile_retire(_profile: &str) -> AppResult<u32> {
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
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "pip_prepare");
    let started = std::time::Instant::now();
    match animehub_android::enter_pip(operation_id.as_str().to_owned(), aspect_num, aspect_den) {
        Ok(entered) => {
            record_bridge_result(
                &operation_id,
                "bridge",
                "pip_enter",
                started,
                if entered { "entered" } else { "not_entered" },
                None,
            );
            Ok(entered)
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "pip_enter",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other("PiP entry failed".into()))
        }
    }
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
    let operation_id = crate::diagnostics::current_or_new_operation_id();
    record_bridge_start(&operation_id, "bridge", "capability");
    let started = std::time::Instant::now();
    match animehub_android::set_pip_auto_enter(operation_id.as_str().to_owned(), enabled) {
        Ok(applied) => {
            record_bridge_result(
                &operation_id,
                "bridge",
                "capability",
                started,
                if applied { "supported" } else { "unsupported" },
                if applied { None } else { Some("unsupported") },
            );
            Ok(applied)
        }
        Err(error) => {
            let class = plugin_error_class(&error);
            record_bridge_result(
                &operation_id,
                "bridge",
                "capability",
                started,
                "failed",
                Some(class),
            );
            Err(AppError::Other("PiP auto-enter setting failed".into()))
        }
    }
}

#[cfg(not(target_os = "android"))]
pub fn set_pip_auto_enter(_enabled: bool) -> AppResult<bool> {
    Ok(false)
}

/// The merged Rust+Kotlin diagnostic timeline, empty when collection is gated off.
#[cfg(target_os = "android")]
pub fn debug_log() -> AppResult<animehub_android::DebugLogSnapshot> {
    let kotlin = animehub_android::debug_log()
        .map_err(|_| AppError::Other("diagnostic bridge read failed".into()))?;
    Ok(crate::diagnostics::snapshot(kotlin))
}

#[cfg(not(target_os = "android"))]
pub fn debug_log() -> AppResult<animehub_android::DebugLogSnapshot> {
    Ok(crate::diagnostics::empty_snapshot())
}

#[cfg(target_os = "android")]
pub fn debug_log_reset() -> AppResult<()> {
    animehub_android::debug_log_reset()
        .map_err(|_| AppError::Other("diagnostic reset failed".into()))
}

#[cfg(not(target_os = "android"))]
pub fn debug_log_reset() -> AppResult<()> {
    Ok(())
}

#[cfg(target_os = "android")]
pub fn set_debug_logging(enabled: bool) -> AppResult<()> {
    animehub_android::set_debug_logging(enabled)
        .map_err(|_| AppError::Other("diagnostic setting could not be applied".into()))
}

#[cfg(not(target_os = "android"))]
pub fn set_debug_logging(_enabled: bool) -> AppResult<()> {
    Ok(())
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
        assert!(
            quarantined.to_string().contains("vazgeçirmez"),
            "the message must say a later successful clear does not lift the wait"
        );

        // Every wording the Kotlin ledger can produce must classify the same
        // way: several outstanding operations, and a verification that
        // predates the last settle (a later clear succeeded while an earlier
        // deletion was still in flight). The user gets guidance, never the raw
        // Kotlin text and never the sentinel.
        for reason in [
            "çerez temizliği zaman aşımına uğradı, işlem doğrulanamadı (+2 bekleyen işlem daha)",
            "son doğrulama, bekleyen son çerez silme işlemi sonuçlanmadan önce yapıldı",
        ] {
            let refusal = classify_site_open_error(animehub_android::Error::Bridge(format!(
                "{}: {}",
                animehub_android::QUARANTINE_SENTINEL,
                reason
            )));
            assert_eq!(refusal.code(), "quarantined", "reason: {reason}");
            assert_eq!(
                refusal.to_string(),
                PROFILE_QUARANTINED_MSG,
                "reason: {reason}"
            );
            assert!(
                !refusal
                    .to_string()
                    .contains(animehub_android::QUARANTINE_SENTINEL),
                "reason: {reason}"
            );
        }

        // Unclassified provider text is never forwarded to the UI or the
        // diagnostics panel.
        let other = classify_site_open_error(animehub_android::Error::Bridge(
            "site açılamadı: private.example token=secret".to_string(),
        ));
        assert_eq!(other.code(), "other");
        assert_eq!(
            other.to_string(),
            "Beklenmeyen hata: Android site profile operation failed"
        );
        assert!(!other.to_string().contains("private.example"));
        assert!(!other.to_string().contains("secret"));

        // Migration retry retains only its stable sentinel so the generation
        // retirement/re-import protocol remains unchanged, never the provider
        // exception detail.
        let retry = classify_site_open_error(animehub_android::Error::Bridge(format!(
            "{}: cookie=secret private.example",
            animehub_android::MIGRATION_RETRY_SENTINEL
        )));
        assert_eq!(retry.code(), "other");
        assert!(retry.to_string().contains("migration-partial"), "{retry}");
        assert!(!retry.to_string().contains("private.example"));
        assert!(!retry.to_string().contains("secret"));
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
