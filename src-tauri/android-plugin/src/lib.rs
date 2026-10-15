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
pub const CMD_SITE_OPEN: &str = "site_open";
pub const CMD_SITE_CLOSE: &str = "site_close";
pub const CMD_SITE_PROFILE_DELETE: &str = "site_profile_delete";
pub const CMD_SITE_CLEAR_DATA: &str = "site_clear_data";
pub const CMD_SITE_FLUSH: &str = "site_flush";
pub const CMD_SITE_PROFILE_NAMES: &str = "site_profile_names";

/// Kotlin's `AnimeHubSiteController.ERR_UNSUPPORTED_MULTI_PROFILE`. A bridge
/// error containing this string means the WebView provider cannot isolate
/// sites; callers must surface user-facing guidance, never retry on the
/// shared profile.
pub const MULTI_PROFILE_SENTINEL: &str = "unsupported-multi-profile";

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

/// Wire format for `site_open`.
///
/// Intentionally not `Debug`: `cookies` carries the one-time legacy jar.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SiteOpenPayload {
    url: String,
    profile: String,
    init_script: String,
    cookies: Vec<String>,
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

/// Kotlin's `{migrated}` reply from `site_open`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteOpenResult {
    /// `true` when the legacy cookie jar was imported into the profile.
    pub migrated: bool,
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

/// Open `url` in a new site WebView bound to `profile`.
///
/// `cookies` (the decrypted 0.3.4 jar, as `Set-Cookie` headers) is imported
/// only by Kotlin when the profile does not exist yet, through the profile's
/// own cookie manager, and only after the callbacks and `flush()` complete —
/// i.e. before the first navigation. An empty list skips the migration.
pub fn site_open(
    url: String,
    profile: String,
    init_script: String,
    cookies: Vec<String>,
) -> Result<SiteOpenResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_SITE_OPEN,
            SiteOpenPayload {
                url,
                profile,
                init_script,
                cookies,
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (url, profile, init_script, cookies);
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
            assert!(matches!(
                site_open(
                    "https://example.com/".into(),
                    "site-00000000000000000000000000000000-g0".into(),
                    String::new(),
                    Vec::new(),
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
        assert_eq!(CMD_SITE_OPEN, "site_open");
        assert_eq!(CMD_SITE_CLOSE, "site_close");
        assert_eq!(CMD_SITE_PROFILE_DELETE, "site_profile_delete");
        assert_eq!(CMD_SITE_CLEAR_DATA, "site_clear_data");
        assert_eq!(CMD_SITE_FLUSH, "site_flush");
        assert_eq!(CMD_SITE_PROFILE_NAMES, "site_profile_names");
        assert_eq!(MULTI_PROFILE_SENTINEL, "unsupported-multi-profile");
        assert_eq!(PLUGIN_ALIAS, "animehub-android");
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
            CMD_SITE_OPEN,
            CMD_SITE_CLOSE,
            CMD_SITE_PROFILE_DELETE,
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
        // No silent fallback: an unsupported provider must reject, not open.
        assert!(
            controller.contains("onResult(false, ERR_UNSUPPORTED_MULTI_PROFILE)"),
            "unsupported providers must fail the open"
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
        // stray call can never reach the Default profile.
        assert!(
            controller.contains("Regex(\"\"\"site-[0-9a-f]{32}-g\\d{1,10}\"\"\")"),
            "profile names must be validated before use"
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
