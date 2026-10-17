//! Android-side bridges for AnimeHub.
//!
//! Four Android platform features need the JVM bridge:
//!
//! * **AndroidKeyStore** — an AES/GCM key that never leaves the secure
//!   hardware. Rust hands over plaintext and gets ciphertext back, so the raw
//!   key is never present in this process.
//! * **Picture-in-Picture** — `PictureInPictureParams` /
//!   `enterPictureInPictureMode()` live on the host `Activity`.
//! * **Per-site WebView profiles** — each registered site opens in its own
//!   native WebView bound to its own androidx.webkit `Profile` (own
//!   localStorage, IndexedDB, cookies), through the `site_*` commands. The
//!   Kotlin half refuses to open when the WebView provider lacks
//!   `WebViewFeature.MULTI_PROFILE` — there is no silent fallback to the
//!   shared default profile.
//!
//! The Kotlin half lives at
//! `src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt` and is
//! copied next to the generated `MainActivity.kt` by `scripts/android_prepare.py`.
//! On every other platform these calls return [`Error::Unsupported`], which
//! keeps the desktop build honest about what it can and cannot do.

use serde::{Deserialize, Serialize};

/// Error type shared with the Kotlin side.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Android bridge çağrılamadı: {0}")]
    Bridge(String),
    #[error("Bu özellik yalnızca Android'de kullanılabilir")]
    Unsupported,
    #[error("Keystore hatası: {0}")]
    Keystore(String),
    #[error("PiP kullanılamıyor: {0}")]
    Pip(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Shape returned by the Kotlin Keystore functions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeystoreResult {
    pub value: String,
}

/// Shape returned by Kotlin commands that report a boolean outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkResult {
    pub ok: bool,
}

// ---------------------------------------------------------------------------
// Mobile command names — must match the `@Command` methods in the Kotlin file.
// ---------------------------------------------------------------------------

pub const CMD_KEYSTORE_SEAL: &str = "keystore_seal";
pub const CMD_KEYSTORE_OPEN: &str = "keystore_open";
pub const CMD_ENTER_PIP: &str = "enter_pip";
pub const CMD_SET_PIP_AUTO_ENTER: &str = "set_pip_auto_enter";
pub const CMD_DEBUG_LOG: &str = "debug_log";
pub const CMD_SITE_MIGRATE: &str = "site_migrate";
pub const CMD_SITE_OPEN: &str = "site_open";
pub const CMD_SITE_CLOSE: &str = "site_close";
pub const CMD_SITE_PROFILE_DELETE: &str = "site_profile_delete";

/// Retire a profile name's quarantine ledger — see [`site_profile_retire`].
pub const CMD_SITE_PROFILE_RETIRE: &str = "site_profile_retire";
pub const CMD_SITE_CLEAR_DATA: &str = "site_clear_data";
pub const CMD_SITE_FLUSH: &str = "site_flush";
pub const CMD_SITE_PROFILE_NAMES: &str = "site_profile_names";

/// Kotlin's `AnimeHubSiteController.ERR_UNSUPPORTED_MULTI_PROFILE`. A bridge
/// error containing this string means the WebView provider cannot isolate
/// sites; callers must surface user-facing guidance, never retry on the
/// shared profile.
pub const MULTI_PROFILE_SENTINEL: &str = "unsupported-multi-profile";

/// Kotlin's `SiteProfileErrors.MIGRATION_RETRY` / `ERR_MIGRATION_RETRY`.
/// A bridge error containing this string means the one-time legacy-cookie
/// import did NOT complete against a virgin profile (a cookie was rejected,
/// an exception fired, or the flush never landed). The open flow retires
/// that profile name and retries; callers must never treat such an attempt
/// as success or as an already-migrated state.
pub const MIGRATION_RETRY_SENTINEL: &str = "migration-partial";

/// Kotlin's `SiteProfileErrors.QUARANTINED`. A bridge error containing this
/// string means the profile's last in-place wipe timed out on a destructive
/// provider call that CANNOT be cancelled (`removeAllCookies`), so its
/// completion is unverified: the deletion may still land after the wipe was
/// reported failed, on data belonging to a session that reopened the same
/// profile. Kotlin refuses both open phases for that name until the wipe is
/// verified (a retry that completes, or a deletion that retires the name), so
/// callers surface guidance instead of retrying the open behind the gate.
///
/// Deliberately distinct from [`MIGRATION_RETRY_SENTINEL`]: a quarantine is
/// NOT a request to retire the profile and re-import the legacy jar — the
/// profile is the user's live one and the wipe simply has to be verified.
pub const QUARANTINE_SENTINEL: &str = "profile-quarantined";

/// Alias under which the Kotlin side is registered.
pub const PLUGIN_ALIAS: &str = "animehub-android";

/// Encrypt `plaintext_b64` with the Keystore key for `purpose`.
///
/// `purpose` becomes part of the key alias, so a blob sealed for one purpose
/// cannot be opened with another.
pub fn keystore_seal(purpose: String, plaintext_b64: String) -> Result<KeystoreResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_KEYSTORE_SEAL,
            PurposeValuePayload {
                purpose,
                value: plaintext_b64,
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (purpose, plaintext_b64);
        Err(Error::Unsupported)
    }
}

/// Decrypt a blob produced by [`keystore_seal`].
pub fn keystore_open(purpose: String, sealed: String) -> Result<KeystoreResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_KEYSTORE_OPEN,
            PurposeValuePayload {
                purpose,
                value: sealed,
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (purpose, sealed);
        Err(Error::Unsupported)
    }
}

// ---------------------------------------------------------------------------
// Per-site WebView profile commands (Android only).
// ---------------------------------------------------------------------------

/// Wire format for `site_migrate` (phase 1 of the two-phase open protocol).
///
/// Intentionally not `Debug`: `cookies` carries the one-time legacy jar.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SiteMigratePayload {
    url: String,
    profile: String,
    cookies: Vec<String>,
}

/// Wire format for `site_open` (phase 2: the view only).
///
/// No cookies: the one-time jar import runs through [`site_migrate`] before
/// the caller persists the migration flag, so this call can never migrate
/// behind that flag's back.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SiteOpenPayload {
    url: String,
    profile: String,
    init_script: String,
}

/// Wire format for `site_close`.
#[cfg(target_os = "android")]
#[derive(Serialize)]
struct SiteClosePayload {
    flush: bool,
}

/// Wire format for `site_profile_delete` / `site_clear_data`.
#[cfg(target_os = "android")]
#[derive(Serialize)]
struct SiteProfilePayload {
    profile: String,
}

/// Kotlin's `{deleted, pending}` reply from `site_profile_delete`.
///
/// * `deleted=true` — the provider accepted the deletion (disk work may
///   finish asynchronously).
/// * `deleted=false, pending=false` — the profile does not exist.
/// * `deleted=false, pending=true` — in use in this process or the provider
///   lacks the API; the caller must queue the name for the next start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteProfileDeleteResult {
    pub deleted: bool,
    pub pending: bool,
}

/// Kotlin's `{dropped}` reply from `site_profile_retire`: how many outstanding
/// destructive-operation records went with the retired name (0 = the ledger was
/// already empty, which is normal — most deletions never timed out).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteProfileRetireResult {
    pub dropped: u32,
}

/// Kotlin's `{names, unsupported}` reply from `site_profile_names`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteProfileNamesResult {
    pub names: Vec<String>,
    /// Provider cannot enumerate profiles (MULTI_PROFILE unsupported).
    #[serde(default)]
    pub unsupported: bool,
}

/// Whether a bridge error is Kotlin's "MULTI_PROFILE unsupported" sentinel.
pub fn is_unsupported_multi_profile(error: &Error) -> bool {
    match error {
        Error::Bridge(message) => message.contains(MULTI_PROFILE_SENTINEL),
        _ => false,
    }
}

/// See [`MIGRATION_RETRY_SENTINEL`]: an incomplete virgin-profile cookie
/// migration that must be retired (queue + generation bump) and retried.
pub fn is_migration_retry(error: &Error) -> bool {
    match error {
        Error::Bridge(message) => message.contains(MIGRATION_RETRY_SENTINEL),
        _ => false,
    }
}

/// See [`QUARANTINE_SENTINEL`]: the profile's last in-place wipe could not be
/// verified, so Kotlin refuses to open it until it is. Distinct from
/// [`is_migration_retry`] — neither sentinel contains the other, so a
/// quarantine refusal can never trigger the retire-and-reimport path.
pub fn is_profile_quarantined(error: &Error) -> bool {
    match error {
        Error::Bridge(message) => message.contains(QUARANTINE_SENTINEL),
        _ => false,
    }
}

/// Phase 1 of the two-phase open: import the one-time legacy jar into
/// `profile` WITHOUT creating the site WebView.
///
/// `cookies` is the decrypted 0.3.4 jar as `Set-Cookie` headers. Kotlin
/// writes them through the profile's own cookie manager and only reports
/// success when every callback accepted AND `flush()` completed — before the
/// first navigation, because no navigation happens in this phase at all.
/// An empty list is a success without touching the provider (nothing to
/// import). A failure carries [`MIGRATION_RETRY_SENTINEL`] (retire the name,
/// retry on a fresh generation), [`MULTI_PROFILE_SENTINEL`] (feature gap) or
/// [`QUARANTINE_SENTINEL`] (this profile's last wipe is unverified, so the
/// jar is not written into it and the flag is not persisted).
///
/// The caller persists its migration flag only after this resolves, and only
/// then calls [`site_open`]: a view the user has seen can never exist while
/// the flag is missing.
pub fn site_migrate(url: String, profile: String, cookies: Vec<String>) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_SITE_MIGRATE,
            SiteMigratePayload {
                url,
                profile,
                cookies,
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (url, profile, cookies);
        Err(Error::Unsupported)
    }
}

/// Phase 2: open `url` in a new site WebView bound to `profile`.
///
/// Takes no cookies — the jar import happened in [`site_migrate`], before
/// the caller persisted the migration flag. A failure may carry
/// [`QUARANTINE_SENTINEL`]: Kotlin refuses to bind a WebView to a profile
/// whose last in-place wipe is still unverified, because the provider call
/// that timed out cannot be cancelled and may still mutate it.
pub fn site_open(url: String, profile: String, init_script: String) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_SITE_OPEN,
            SiteOpenPayload {
                url,
                profile,
                init_script,
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (url, profile, init_script);
        Err(Error::Unsupported)
    }
}

/// Close (and flush unless `flush=false`) the active site WebView.
/// Idempotent: no active site resolves normally.
pub fn site_close(flush: bool) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SITE_CLOSE, SiteClosePayload { flush })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = flush;
        Err(Error::Unsupported)
    }
}

/// Delete a site profile's data (never the default profile — Kotlin validates
/// the name against `android_profile_name`'s shape).
pub fn site_profile_delete(profile: String) -> Result<SiteProfileDeleteResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SITE_PROFILE_DELETE, SiteProfilePayload { profile })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = profile;
        Err(Error::Unsupported)
    }
}

/// Retire one profile name's quarantine ledger, dropping every outstanding
/// destructive-operation record for it.
///
/// CALL ORDER IS THE SAFETY PROPERTY, and it belongs to the caller: this must
/// run only AFTER the site's generation bump is durably stored
/// (`run_mobile_delete_protocol`), because that bump is what makes the name
/// underivable forever. Retiring on deletion *acceptance* — as Kotlin used to do
/// inside `site_profile_delete` — left a window where a failed bump or a failed
/// storage write aborted `remove_site` with the registry intact, the next open
/// derived the SAME name, and the gate no longer knew about a
/// `removeAllCookies` deletion that could still be in flight on it.
pub fn site_profile_retire(profile: String) -> Result<SiteProfileRetireResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SITE_PROFILE_RETIRE, SiteProfilePayload { profile })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = profile;
        Err(Error::Unsupported)
    }
}

/// Clear one site profile in place (cookies, web storage, permissions).
pub fn site_clear_data(profile: String) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SITE_CLEAR_DATA, SiteProfilePayload { profile })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = profile;
        Err(Error::Unsupported)
    }
}

/// Flush the active site profile's cookie jar (periodic durability sync).
pub fn site_flush() -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SITE_FLUSH, NoArgs {})
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(Error::Unsupported)
    }
}

/// Every profile name the provider store knows, or `unsupported=true`.
pub fn site_profile_names() -> Result<SiteProfileNamesResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SITE_PROFILE_NAMES, NoArgs {})
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(Error::Unsupported)
    }
}

/// Enter Picture-in-Picture with the given aspect ratio as the fallback.
///
/// The Kotlin side prepares the shared WebView first (`AnimeHubPipController`
/// calls the site's `window.__animehubPreparePip()` and waits for its answer),
/// so the PiP window shows the active player instead of the whole site. The
/// ratio here is only used when the page could not report the player's own.
pub fn enter_pip(aspect_num: u32, aspect_den: u32) -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        call_plugin::<_, OkResult>(
            CMD_ENTER_PIP,
            PipPayload {
                num: aspect_num,
                den: aspect_den,
            },
        )
        .map(|r| r.ok)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (aspect_num, aspect_den);
        Err(Error::Unsupported)
    }
}

/// Empty argument object for commands that take none.
#[cfg(target_os = "android")]
#[derive(Serialize)]
struct NoArgs {}

#[cfg(target_os = "android")]
#[derive(Deserialize)]
struct DebugLogResponse {
    lines: Vec<String>,
}

/// The last PiP diagnostic lines kept by the Kotlin side (in-app log; no
/// logcat needed). Read-only: it never changes PiP state.
pub fn debug_log() -> Result<Vec<String>> {
    #[cfg(target_os = "android")]
    {
        call_plugin::<_, DebugLogResponse>(CMD_DEBUG_LOG, NoArgs {}).map(|r| r.lines)
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(Error::Unsupported)
    }
}

/// Enable or disable the controlled auto-PiP paths (never platform auto-enter).
///
/// See `AnimeHubPipController.applyAutoEnter`: `setAutoEnterEnabled(true)`
/// would let the system skip the WebView preparation entirely, so the setting
/// only gates `onPictureInPictureRequested()` / `onUserLeaveHint()`.
pub fn set_pip_auto_enter(enabled: bool) -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        call_plugin::<_, OkResult>(CMD_SET_PIP_AUTO_ENTER, AutoEnterPayload { enabled })
            .map(|r| r.ok)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = enabled;
        Err(Error::Unsupported)
    }
}

/// Type-erased mobile invoke.
///
/// `tauri::plugin::mobile::PluginHandle` is private, and `init` is generic
/// over the runtime, so the handle cannot be stored as `PluginHandle<Wry>`.
/// The closure owns whatever `register_android_plugin` returned.
#[cfg(target_os = "android")]
type MobileCall =
    dyn Fn(&str, serde_json::Value) -> std::result::Result<serde_json::Value, String> + Send;

/// The registered plugin handle.
///
/// Set exactly once by [`init`]. The free functions above are called from
/// plain Rust (no `AppHandle` in scope), so the handle has to live in a
/// global; it is only ever written during setup, before any command runs.
#[cfg(target_os = "android")]
static HANDLE: std::sync::OnceLock<std::sync::Mutex<Option<Box<MobileCall>>>> =
    std::sync::OnceLock::new();

/// Wire format for `{ purpose, value }` commands (keystore seal/open).
#[cfg(target_os = "android")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PurposeValuePayload {
    purpose: String,
    value: String,
}

/// Wire format for `enter_pip`.
#[cfg(target_os = "android")]
#[derive(Debug, Serialize)]
struct PipPayload {
    num: u32,
    den: u32,
}

/// Wire format for `set_pip_auto_enter`.
#[cfg(target_os = "android")]
#[derive(Debug, Serialize)]
struct AutoEnterPayload {
    enabled: bool,
}

/// Send a command to the Kotlin plugin and return its decoded response.
///
/// Uses `PluginHandle::run_mobile_plugin`, which serialises `payload` and
/// deserialises the `JSObject` the Kotlin side resolves with. The call blocks
/// the current thread until Kotlin resolves the invoke — that is what makes
/// the JS-evaluation round-trips usable from synchronous Rust.
#[cfg(target_os = "android")]
fn call_plugin<P: serde::Serialize, R: serde::de::DeserializeOwned>(
    cmd: &str,
    payload: P,
) -> Result<R> {
    let guard = HANDLE
        .get()
        .ok_or_else(|| Error::Bridge("plugin henüz başlatılmamış".into()))?
        .lock()
        .map_err(|_| Error::Bridge("handle kilidi zehirlenmiş".into()))?;
    let handle = guard
        .as_ref()
        .ok_or_else(|| Error::Bridge("plugin henüz başlatılmamış".into()))?;

    let payload = serde_json::to_value(payload).map_err(|e| Error::Bridge(e.to_string()))?;
    let value = handle(cmd, payload).map_err(Error::Bridge)?;
    serde_json::from_value(value).map_err(|e| Error::Bridge(e.to_string()))
}

/// Register the plugin with a Tauri app.
///
/// On Android this wires the Kotlin implementation; elsewhere it registers a
/// no-op so the same `Builder` chain compiles for every target.
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new(PLUGIN_ALIAS)
        .setup(|_app, api| {
            #[cfg(target_os = "android")]
            {
                // Tauri turns the identifier `dev.animehub.app` into the Kotlin
                // package `dev.animehub.app` (dots stay dots; they are not
                // rewritten to underscores). `scripts/android_prepare.py`
                // refuses to continue if the generated MainActivity package
                // disagrees with this string.
                let handle = api.register_android_plugin("dev.animehub.app", "AnimeHubPlugin")?;
                let call: Box<MobileCall> = Box::new(move |cmd, payload| {
                    handle
                        .run_mobile_plugin(cmd, payload)
                        .map_err(|e| e.to_string())
                });
                let slot = HANDLE.get_or_init(|| std::sync::Mutex::new(None));
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(call);
                } else {
                    return Err(Box::from("Android plugin handle kaydedilemedi"));
                }
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = api;
            }
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_calls_report_unsupported() {
        // This crate is compiled on every target; on desktop the bridges must
        // fail loudly rather than pretend to have worked.
        if cfg!(not(target_os = "android")) {
            assert!(matches!(
                keystore_seal("p".into(), "eA==".into()),
                Err(Error::Unsupported)
            ));
            assert!(matches!(
                keystore_open("p".into(), "eA==".into()),
                Err(Error::Unsupported)
            ));
            assert!(matches!(enter_pip(16, 9), Err(Error::Unsupported)));
            assert!(matches!(set_pip_auto_enter(true), Err(Error::Unsupported)));
            // Both phases of the two-phase open fail honestly off Android.
            assert!(matches!(
                site_migrate(
                    "https://example.com/".into(),
                    "site-00000000000000000000000000000000-g0".into(),
                    Vec::new(),
                ),
                Err(Error::Unsupported)
            ));
            assert!(matches!(
                site_open(
                    "https://example.com/".into(),
                    "site-00000000000000000000000000000000-g0".into(),
                    String::new(),
                ),
                Err(Error::Unsupported)
            ));
            assert!(matches!(site_close(true), Err(Error::Unsupported)));
            assert!(matches!(
                site_profile_delete("site-00000000000000000000000000000000-g0".into()),
                Err(Error::Unsupported)
            ));
        }
    }

    #[test]
    fn keystore_result_deserialises_from_kotlin_camelcase() {
        let json = r#"{"value":"SGVsbG8="}"#;
        let r: KeystoreResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.value, "SGVsbG8=");
    }

    #[test]
    fn ok_result_deserialises_from_kotlin() {
        let r: OkResult = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(r.ok);
    }

    #[test]
    fn error_messages_are_user_presentable() {
        let e = Error::Keystore("anahtar yok".into());
        assert_eq!(e.to_string(), "Keystore hatası: anahtar yok");
        assert_eq!(
            Error::Unsupported.to_string(),
            "Bu özellik yalnızca Android'de kullanılabilir"
        );
    }

    #[test]
    fn command_names_are_stable() {
        // The Kotlin side matches on these strings; a rename on one side only
        // would fail at runtime, so pin them.
        assert_eq!(CMD_KEYSTORE_SEAL, "keystore_seal");
        assert_eq!(CMD_KEYSTORE_OPEN, "keystore_open");
        assert_eq!(CMD_ENTER_PIP, "enter_pip");
        assert_eq!(CMD_SET_PIP_AUTO_ENTER, "set_pip_auto_enter");
        assert_eq!(CMD_DEBUG_LOG, "debug_log");
        // Two-phase open: phase 1 (jar import, no view) runs BEFORE Rust
        // persists the migration flag; phase 2 builds the view.
        assert_eq!(CMD_SITE_MIGRATE, "site_migrate");
        assert_eq!(CMD_SITE_OPEN, "site_open");
        assert_eq!(CMD_SITE_CLOSE, "site_close");
        assert_eq!(CMD_SITE_PROFILE_DELETE, "site_profile_delete");
        assert_eq!(CMD_SITE_CLEAR_DATA, "site_clear_data");
        assert_eq!(CMD_SITE_FLUSH, "site_flush");
        assert_eq!(CMD_SITE_PROFILE_NAMES, "site_profile_names");
        assert_eq!(MULTI_PROFILE_SENTINEL, "unsupported-multi-profile");
        assert_eq!(MIGRATION_RETRY_SENTINEL, "migration-partial");
        assert_eq!(QUARANTINE_SENTINEL, "profile-quarantined");
        // The classifier must key off exactly the const Kotlin produces.
        assert!(is_migration_retry(&Error::Bridge(
            "migration-partial: çerez reddedildi (1/2)".into()
        )));
        assert!(!is_migration_retry(&Error::Bridge("something else".into())));
        assert!(is_profile_quarantined(&Error::Bridge(
            "profile-quarantined: çerez temizliği zaman aşımına uğradı, işlem doğrulanamadı".into()
        )));
        assert!(!is_profile_quarantined(&Error::Bridge(
            "something else".into()
        )));
        // The two sentinels must stay disjoint: a quarantine refusal must
        // never trigger the retire-and-reimport path (which would delete the
        // user's live profile and rebuild it from the stale jar), and a
        // migration retry must never be read as a quarantine.
        assert!(
            !is_migration_retry(&Error::Bridge(format!(
                "{QUARANTINE_SENTINEL}: çerez temizliği zaman aşımına uğradı"
            ))),
            "a quarantine refusal is not a migration retry"
        );
        assert!(
            !is_profile_quarantined(&Error::Bridge(format!(
                "{MIGRATION_RETRY_SENTINEL}: profil zaten mevcut"
            ))),
            "a migration retry is not a quarantine"
        );
        assert_eq!(PLUGIN_ALIAS, "animehub-android");
    }

    #[test]
    fn kotlin_pins_the_late_callback_guard_and_quarantine_contract() {
        // The pure Kotlin side is split by responsibility (SiteProfileOps.kt's
        // header lists the layout). `include_str!` fails the BUILD when a file
        // goes missing, so a dropped file cannot silently shrink what is pinned.
        let ops = concat!(
            include_str!("../kotlin/dev/animehub/app/SiteProfileOps.kt"),
            include_str!("../kotlin/dev/animehub/app/ProfileQuarantine.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteCloseFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteShutdownFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteClearFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteMigrationFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteBackStatus.kt"),
        );
        let controller = include_str!("../kotlin/dev/animehub/app/AnimeHubSiteController.kt");
        // The literal Rust classifies is the one Kotlin produces.
        assert!(
            ops.contains(&format!(
                "const val QUARANTINED: String = \"{QUARANTINE_SENTINEL}\""
            )),
            "the quarantine sentinel must match the Kotlin constant"
        );
        // The awaited geolocation wipe is GATED: the guard is claimed before
        // the first mutation, and a callback that lost to the watchdog clears
        // nothing and reports nothing.
        assert!(
            ops.contains(
                "fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit)"
            ),
            "the clear port must hand the step guard to the geolocation wipe"
        );
        assert!(
            ops.contains("if (!guard.claim()) return@getOrigins"),
            "a late callback must be refused BEFORE any geo.clear runs"
        );
        assert!(
            ops.contains("for (origin in origins) api.clear(origin)"),
            "the mutation must sit behind the claim, in the pure wipe"
        );
        // The destructive call that CANNOT be gated is registered instead —
        // under its OWN operation id, so no later attempt can clear it.
        assert!(
            ops.contains("fun quarantine(profileName: String, operationId: Long, reason: String)"),
            "the registry must be per destructive operation, not per profile name"
        );
        assert!(
            ops.contains(
                "ProfileQuarantine.quarantine(profileName, guard.operationId, COOKIE_UNVERIFIED_REASON)"
            ),
            "an unverified removeAllCookies must register its own operation"
        );
        assert!(
            ops.contains("ProfileQuarantine.settle(profileName, guard.operationId)"),
            "a late callback may close ONLY the operation it belongs to"
        );
        assert!(
            ops.contains("ProfileQuarantine.verified(profileName)"),
            "a verified wipe records a verification; it must not release the name"
        );
        // A blanket release/reset would erase other operations' records, which
        // is exactly the race the per-operation ledger exists to prevent.
        assert!(
            !ops.contains("ProfileQuarantine.release("),
            "the blanket release API must not come back"
        );
        assert!(
            !ops.contains("ProfileQuarantine.reset()")
                && !controller.contains("ProfileQuarantine.reset()"),
            "production Kotlin must never reset the ledger"
        );
        // Retirement is a SEPARATE, Rust-sequenced step: the ledger may only be
        // dropped once the generation bump is durable, so an accepted deletion
        // must NOT retire the name by itself.
        assert!(
            controller.contains("fun retireProfileQuarantine(profileName: String"),
            "the controller must expose retirement as its own entry point"
        );
        assert_eq!(
            controller
                .matches("ProfileQuarantine.retire(profileName)")
                .count(),
            1,
            "exactly one retirement call site: inside retireProfileQuarantine"
        );
        {
            let delete_fn = &controller[controller
                .find("fun deleteProfileData(")
                .expect("deleteProfileData must exist")
                ..controller
                    .find("fun retireProfileQuarantine(")
                    .expect("retireProfileQuarantine must exist")];
            assert!(
                !delete_fn.contains("ProfileQuarantine.retire("),
                "deleteProfileData must not retire on mere acceptance by the provider"
            );
        }
        for marker in [
            "object ProfileRetirement",
            "fun refusal(profileName: String, activeProfile: String?): String?",
            "!SITE_PROFILE_NAME.matches(profileName) -> INVALID_NAME",
            "activeProfile == profileName -> ACTIVE_PROFILE",
            "val SITE_PROFILE_NAME: Regex",
        ] {
            assert!(
                ops.contains(marker),
                "the retirement decision must stay pure and keep: {marker}"
            );
        }
        assert!(
            !controller.contains("Regex(\"\"\"site-"),
            "the profile-name rule must not be duplicated in the controller"
        );
        assert!(
            controller.contains("ProfileQuarantine.isBlocked(profileName)"),
            "the clear result must report the block through the new predicate"
        );
        // Reopenability is a PROVEN end state: nothing outstanding, and a
        // verification newer than the last settle.
        for marker in [
            "val pending = LinkedHashMap<Long, String>()",
            "STALE_VERIFICATION_REASON",
            "fun isBlocked(profileName: String): Boolean",
            "fun pendingOperations(profileName: String): List<Long>",
            "if (state.verifiedAt < state.lastSettledAt) return STALE_VERIFICATION_REASON",
            "fun settle(profileName: String, operationId: Long): Boolean",
            "fun retire(profileName: String): Int",
        ] {
            assert!(
                ops.contains(marker),
                "the quarantine ledger must keep: {marker}"
            );
        }
        // Both open phases consult the gate before touching the provider, and
        // the real adapter delegates the sequencing to the pure wipe instead
        // of clearing origins inline.
        assert_eq!(
            controller.matches("quarantineRefusal(profileName,").count(),
            2,
            "phase 1 (site_migrate) and phase 2 (site_open) each refuse"
        );
        assert!(
            controller.contains("GeolocationWipe.start("),
            "the adapter must run the production wipe sequencing"
        );
        assert!(
            !controller.contains("for (origin in origins) geo.clear(origin)"),
            "ungated inline geolocation clears must not come back"
        );
        assert!(
            controller.contains("SiteClearFlow.run(isSupported(), profileName, port)"),
            "the flow needs the profile name to quarantine the right profile"
        );
    }

    #[test]
    fn kotlin_pins_the_typed_cookie_wipe_contract() {
        // The pure Kotlin side is split by responsibility (SiteProfileOps.kt's
        // header lists the layout). `include_str!` fails the BUILD when a file
        // goes missing, so a dropped file cannot silently shrink what is pinned.
        let ops = concat!(
            include_str!("../kotlin/dev/animehub/app/SiteProfileOps.kt"),
            include_str!("../kotlin/dev/animehub/app/ProfileQuarantine.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteCloseFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteShutdownFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteClearFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteMigrationFlow.kt"),
            include_str!("../kotlin/dev/animehub/app/SiteBackStatus.kt"),
        );
        let controller = include_str!("../kotlin/dev/animehub/app/AnimeHubSiteController.kt");
        // Chromium's `removeAllCookies` callback reports whether ANY cookie was
        // deleted — an outcome, not an error flag. An already-empty jar answers
        // `false` for a wipe that fully succeeded, while a store that could not
        // be reached also produced `false` in the old boolean shape: clearing a
        // clean profile then failed the whole clear and Rust never reached
        // `delete_site_session`. The port therefore carries a TYPE, and one pure
        // function interprets it.
        for marker in [
            "sealed interface CookieWipe",
            "data class Answered(val deleted: Boolean) : CookieWipe",
            "data class Failed(val reason: String) : CookieWipe",
            "data class CookieStep(val ok: Boolean, val error: String?)",
            "fun removeAllCookies(onDone: (CookieWipe) -> Unit)",
            "fun cookiesCleared(): Boolean?",
            "fun interpretCookieWipe(wipe: CookieWipe, cookiesCleared: () -> Boolean?): CookieStep",
            "COOKIE_STILL_PRESENT_REASON",
            "COOKIE_STATE_UNKNOWN_REASON",
            "COOKIE_ACCESS_FAILED_PREFIX",
            // An empty jar is a success; leftovers and an unreadable store are
            // failures — indeterminate is never laundered into success.
            "true -> CookieStep(true, null)",
            "false -> CookieStep(false, COOKIE_STILL_PRESENT_REASON)",
            "null -> CookieStep(false, COOKIE_STATE_UNKNOWN_REASON)",
            "is CookieWipe.Failed -> CookieStep(false,",
        ] {
            assert!(ops.contains(marker), "the cookie step must keep: {marker}");
        }
        assert!(
            !ops.contains("fun removeAllCookies(onDone: (Boolean) -> Unit)"),
            "the boolean-shaped cookie port must not come back"
        );
        // The adapter passes the raw answer through and keeps real errors
        // typed — via the PURE mapping the CI unit tests execute, so the two
        // inversion-sensitive rules (a null delivery is not a deletion, and
        // `hasCookies()` is the opposite of "jar empty") cannot drift.
        for marker in [
            "object CookieWipes",
            "fun answered(deleted: Boolean?): CookieWipe = CookieWipe.Answered(deleted == true)",
            "fun failed(error: Throwable): CookieWipe =",
            "fun noProfile(): CookieWipe = CookieWipe.Failed(NO_PROFILE_REASON)",
            "fun emptinessOf(hasCookies: () -> Boolean): Boolean? =",
            "runCatching { !hasCookies() }.getOrNull()",
        ] {
            assert!(
                ops.contains(marker),
                "the adapter mapping must keep: {marker}"
            );
        }
        assert!(
            controller.contains("onDone(CookieWipes.answered(deleted))"),
            "the provider's raw answer must reach the flow through the tested mapping"
        );
        assert!(
            controller.contains("onDone(CookieWipes.failed(e))")
                && controller.contains("onDone(CookieWipes.noProfile())"),
            "an unreachable store must arrive as a typed failure, not as false"
        );
        assert!(
            !controller.contains("onDone(ok == true)"),
            "the adapter must not collapse the answer into a success boolean"
        );
        assert!(
            controller.contains("override fun cookiesCleared(): Boolean?"),
            "the adapter must expose the emptiness probe"
        );
        assert!(
            controller.contains("CookieWipes.emptinessOf { profile.cookieManager.hasCookies() }"),
            "the probe must read the profile's own store through the tested inversion"
        );
        // Round 6 must survive this change: the timeout still registers its own
        // operation and a late callback still closes only that record.
        assert!(
            ops.contains(
                "ProfileQuarantine.quarantine(profileName, guard.operationId, COOKIE_UNVERIFIED_REASON)"
            ),
            "a cookie timeout still owns its quarantine record"
        );
        assert!(
            ops.contains("ProfileQuarantine.settle(profileName, guard.operationId)"),
            "a late cookie answer still closes only its own record"
        );
    }

    #[test]
    fn kotlin_pip_controller_template_is_wired_for_controlled_entry() {
        // Template, not a generated file: `scripts/android_prepare.py` fills
        // the JS in and copies it into the Android project.
        let kt = include_str!("../kotlin/dev/animehub/app/AnimeHubPipController.kt");
        for marker in [
            "object AnimeHubPipController",
            "fun attach(",
            "fun detach(",
            "fun requestEnter(",
            "fun restore(",
            "fun applyAutoEnter(",
            "__animehubPreparePip",
            "__animehubPip",
            "setSourceRectHint",
            "enterPictureInPictureMode(",
        ] {
            assert!(
                kt.contains(marker),
                "AnimeHubPipController.kt is missing `{marker}`"
            );
        }
        assert!(
            kt.contains("__ANIMEHUB_PIP_CONTROLLER_JS__"),
            "the JS placeholder must stay for scripts/android_prepare.py"
        );
        assert!(
            !kt.contains("setAutoEnterEnabled(true)"),
            "platform auto-enter would skip the WebView preparation"
        );
    }

    #[test]
    fn pip_commands_delegate_to_the_controller_instead_of_duplicating_it() {
        let kt = include_str!("../kotlin/dev/animehub/app/AnimeHubPlugin.kt");
        assert!(
            kt.contains("AnimeHubPipController.requestEnter("),
            "enter_pip must use the shared preparation chain"
        );
        assert!(
            kt.contains("AnimeHubPipController.applyAutoEnter("),
            "set_pip_auto_enter must go through the controller"
        );
        // The call form, not the word: the doc comment above the command
        // explains the chain it hands over to.
        assert!(
            !kt.contains(".enterPictureInPictureMode("),
            "the plugin must not enter PiP on its own"
        );
        assert!(
            !kt.contains("setAutoEnterEnabled"),
            "platform auto-enter is disabled for good; the controller owns it"
        );
    }

    #[test]
    fn android_plugin_package_matches_the_app_identifier() {
        // `scripts/android_prepare.py` reads this literal. Tauri generates
        // `package dev.animehub.app` from the identifier; underscores would
        // not load.
        let src = include_str!("lib.rs");
        assert!(
            src.contains("register_android_plugin(\"dev.animehub.app\", \"AnimeHubPlugin\")"),
            "registration string drifted from the Kotlin package"
        );
    }

    #[test]
    fn kotlin_pure_files_stay_split_pure_and_packaged() {
        // `SiteProfileOps.kt` was split by responsibility (its header lists the
        // layout). Each file must keep its license header, its package line and
        // the declarations it is the home of — and none of them may import
        // Android or Tauri types, because the CI unit-test source set compiles
        // them against the generated project and runs `SiteProfileOpsTest`
        // through them. A file that disappears fails the build via
        // `include_str!`, so the pinned surface cannot silently shrink.
        let primitives = include_str!("../kotlin/dev/animehub/app/SiteProfileOps.kt");
        let quarantine = include_str!("../kotlin/dev/animehub/app/ProfileQuarantine.kt");
        let close = include_str!("../kotlin/dev/animehub/app/SiteCloseFlow.kt");
        let shutdown = include_str!("../kotlin/dev/animehub/app/SiteShutdownFlow.kt");
        let clear = include_str!("../kotlin/dev/animehub/app/SiteClearFlow.kt");
        let migration = include_str!("../kotlin/dev/animehub/app/SiteMigrationFlow.kt");
        let back = include_str!("../kotlin/dev/animehub/app/SiteBackStatus.kt");
        for (name, src, homes) in [
            (
                "SiteProfileOps.kt",
                primitives,
                &[
                    "object SiteProfileErrors",
                    "class SiteSessionState",
                    "object StepGuards",
                    "object GeolocationWipe",
                ][..],
            ),
            (
                "ProfileQuarantine.kt",
                quarantine,
                &[
                    "object ProfileQuarantine",
                    "object SiteOpenGate",
                    "object ProfileRetirement",
                    "val SITE_PROFILE_NAME",
                ][..],
            ),
            (
                "SiteCloseFlow.kt",
                close,
                &["interface SiteClosePort", "object SiteCloseFlow"][..],
            ),
            (
                "SiteShutdownFlow.kt",
                shutdown,
                &[
                    "sealed interface ShutdownFlush",
                    "interface ShutdownPort",
                    "object ShutdownFlow",
                ][..],
            ),
            (
                "SiteClearFlow.kt",
                clear,
                &[
                    "sealed interface CookieWipe",
                    "object CookieWipes",
                    "interface SiteClearPort",
                    "object SiteClearFlow",
                ][..],
            ),
            (
                "SiteMigrationFlow.kt",
                migration,
                &[
                    "interface SiteMigrationPort",
                    "sealed class MigrationOutcome",
                    "object SiteMigrationFlow",
                ][..],
            ),
            ("SiteBackStatus.kt", back, &["object SiteBackStatus"][..]),
        ] {
            assert!(
                src.starts_with("// SPDX-License-Identifier: MIT"),
                "{name} lost its license header"
            );
            assert!(
                src.contains("package dev.animehub.app"),
                "{name} lost its package line"
            );
            assert!(
                !src.contains("import android."),
                "{name} must stay pure: no android.* import"
            );
            assert!(
                !src.contains("import app.tauri"),
                "{name} must stay pure: no Tauri import"
            );
            for marker in homes {
                assert!(
                    src.contains(marker),
                    "{name} must be the home of `{marker}`"
                );
            }
        }
    }

    #[test]
    fn kotlin_file_declares_every_command() {
        // Command names must exist as `fun <name>` on the Kotlin side; a
        // missing method would fail only at runtime on a device.
        let kt = include_str!("../kotlin/dev/animehub/app/AnimeHubPlugin.kt");
        for cmd in [
            CMD_KEYSTORE_SEAL,
            CMD_KEYSTORE_OPEN,
            CMD_ENTER_PIP,
            CMD_SET_PIP_AUTO_ENTER,
            CMD_DEBUG_LOG,
            CMD_SITE_MIGRATE,
            CMD_SITE_OPEN,
            CMD_SITE_CLOSE,
            CMD_SITE_PROFILE_DELETE,
            CMD_SITE_PROFILE_RETIRE,
            CMD_SITE_CLEAR_DATA,
            CMD_SITE_FLUSH,
            CMD_SITE_PROFILE_NAMES,
        ] {
            let marker = format!("fun {cmd}(");
            assert!(kt.contains(&marker), "Kotlin side is missing `fun {cmd}(`");
        }
        // The legacy JSON storage/cookie transfer commands are gone.
        for gone in [
            "fun localstorage_export(",
            "fun localstorage_import(",
            "fun indexeddb_export(",
            "fun indexeddb_import(",
            "fun webview_cookies_get(",
            "fun webview_cookies_replace(",
        ] {
            assert!(
                !kt.contains(gone),
                "removed command still present on the Kotlin side: {gone}"
            );
        }
        assert!(
            !kt.contains("@JavascriptInterface") && !kt.contains("addJavascriptInterface"),
            "the plugin must not expose any page-facing JavascriptInterface"
        );
    }

    #[test]
    fn site_controller_enforces_the_multi_profile_contract() {
        let controller = include_str!("../kotlin/dev/animehub/app/AnimeHubSiteController.kt");
        // Home of the shared profile-name rule and the retirement decision
        // (split out of SiteProfileOps.kt).
        let quarantine = include_str!("../kotlin/dev/animehub/app/ProfileQuarantine.kt");
        // Runtime support check before anything else happens on open.
        assert!(
            controller.contains("WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)"),
            "open must gate on WebViewFeature.MULTI_PROFILE"
        );
        assert!(
            controller.contains(&format!(
                "const val ERR_UNSUPPORTED_MULTI_PROFILE = \"{MULTI_PROFILE_SENTINEL}\""
            )),
            "the sentinel Rust classifies must match the Kotlin constant"
        );
        // No silent fallback: an unsupported provider must reject, not open —
        // in BOTH phases of the two-phase open (migrate and the view).
        assert!(
            controller.contains("onResult(ERR_UNSUPPORTED_MULTI_PROFILE)"),
            "unsupported providers must fail the open"
        );
        assert_eq!(
            controller
                .matches("onResult(ERR_UNSUPPORTED_MULTI_PROFILE)")
                .count(),
            2,
            "phase 1 (site_migrate) and phase 2 (site_open) each refuse"
        );
        // Profile assignment before any other WebView API call: the constructor
        // is immediately followed by setProfile.
        assert!(
            controller.contains("webView = WebView(activity)") // setProfile follows
                && controller.contains("WebViewCompat.setProfile(webView, profileName)"),
            "setProfile must be called on the fresh WebView"
        );
        // Site cookies only ever touch the profile's own manager.
        assert!(
            controller.contains("profile.cookieManager"),
            "cookie work must go through Profile.getCookieManager()"
        );
        // Profile names are validated against the Rust derivation shape, so a
        // stray call can never reach the Default profile. The rule is defined
        // ONCE — in ProfileQuarantine.kt — and the controller validates with it,
        // so validation and the retirement decision cannot drift apart.
        assert!(
            quarantine.contains(
                "val SITE_PROFILE_NAME: Regex = Regex(\"\"\"site-[0-9a-f]{32}-g\\d{1,10}\"\"\")"
            ),
            "profile names must be validated before use"
        );
        assert!(
            controller.contains("SITE_PROFILE_NAME.matches(name)"),
            "the controller must validate with the shared rule, not a copy of it"
        );
        // Orphan sweep support: enumeration exists and is feature-gated.
        assert!(
            controller.contains("fun allProfileNames(): List<String>?"),
            "the clear-all orphan sweep needs profile enumeration"
        );
        // Back button: site history first, launcher second, never finish().
        assert!(
            controller.contains("webView.canGoBack()")
                && controller.contains("closeViaLauncher(activity)"),
            "back must walk site history before returning to the launcher"
        );
        // The new file must not carry page-facing script interfaces either.
        assert!(
            !controller.contains("@JavascriptInterface")
                && !controller.contains("addJavascriptInterface"),
            "no JavascriptInterface on the site WebView"
        );
    }
}
