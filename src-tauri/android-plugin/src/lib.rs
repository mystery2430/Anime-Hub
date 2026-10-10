//! Android-side bridges for AnimeHub.
//!
//! Four Android platform features need the JVM bridge:
//!
//! * **AndroidKeyStore** — an AES/GCM key that never leaves the secure
//!   hardware. Rust hands over plaintext and gets ciphertext back, so the raw
//!   key is never present in this process.
//! * **Picture-in-Picture** — `PictureInPictureParams` /
//!   `enterPictureInPictureMode()` live on the host `Activity`.
//! * **Per-site cookies** — Android's shared `CookieManager` is read and
//!   replaced through a null-safe, callback-aware Kotlin API.
//! * **Per-site WebView storage** — on Android the system WebView has no
//!   per-profile data directory, so `localStorage` / IndexedDB are
//!   exported/imported (and Keystore-sealed) from Kotlin via
//!   `evaluateJavascript`, the only synchronous-enough channel the app has
//!   into the shared WebView.
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

/// Shape returned by the Kotlin Keystore/storage-export functions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeystoreResult {
    pub value: String,
}

/// Shape returned by Kotlin commands that provide a plain string value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringResult {
    pub value: String,
}

/// Shape returned by the Kotlin storage-import, cookie-replace and PiP functions.
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
pub const CMD_LOCALSTORAGE_EXPORT: &str = "localstorage_export";
pub const CMD_LOCALSTORAGE_IMPORT: &str = "localstorage_import";
pub const CMD_INDEXEDDB_EXPORT: &str = "indexeddb_export";
pub const CMD_INDEXEDDB_IMPORT: &str = "indexeddb_import";
pub const CMD_WEBVIEW_COOKIES_GET: &str = "webview_cookies_get";
pub const CMD_WEBVIEW_COOKIES_REPLACE: &str = "webview_cookies_replace";

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

/// Export the shared WebView's `localStorage` as Keystore-sealed base64.
///
/// Blocks until the page's JS has been evaluated and the blob sealed, so the
/// caller can rely on the export being complete when this returns. The page
/// must still be the site whose storage is being exported — call this before
/// navigating away.
pub fn localstorage_export(purpose: String) -> Result<KeystoreResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_LOCALSTORAGE_EXPORT, PurposePayload { purpose })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = purpose;
        Err(Error::Unsupported)
    }
}

/// Restore a [`localstorage_export`] blob into the currently loaded page.
pub fn localstorage_import(purpose: String, sealed: String) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_LOCALSTORAGE_IMPORT,
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

/// Export the shared WebView's IndexedDB databases (best effort: document
/// values only — indexes, key paths and exotic key types are not preserved).
pub fn indexeddb_export(purpose: String) -> Result<KeystoreResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_INDEXEDDB_EXPORT, PurposePayload { purpose })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = purpose;
        Err(Error::Unsupported)
    }
}

/// Restore an [`indexeddb_export`] blob into the currently loaded page.
pub fn indexeddb_import(purpose: String, sealed: String) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_INDEXEDDB_IMPORT,
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

/// Read the live Android WebView's cookie header for `url`.
///
/// Unlike Wry's Android `cookies()` API (which is unsupported), this uses
/// Android's shared CookieManager through the Kotlin plugin. Kotlin maps the
/// normal no-cookies `null` result to an empty string.
pub fn webview_cookies_get(url: String) -> Result<StringResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_WEBVIEW_COOKIES_GET, CookieUrlPayload { url })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = url;
        Err(Error::Unsupported)
    }
}

/// Replace Android's shared CookieManager contents and wait until each
/// accepted cookie has been set before the caller navigates the WebView.
pub fn webview_cookies_replace(url: String, cookies: Vec<String>) -> Result<OkResult> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_WEBVIEW_COOKIES_REPLACE,
            CookieReplacePayload { url, cookies },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (url, cookies);
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

/// Wire format for `{ purpose, value }` commands (keystore seal/open and both
/// storage imports).
#[cfg(target_os = "android")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PurposeValuePayload {
    purpose: String,
    value: String,
}

/// Wire format for the export commands, which only need the key purpose.
#[cfg(target_os = "android")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PurposePayload {
    purpose: String,
}

/// Wire format for reading cookies for one WebView URL.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookieUrlPayload {
    url: String,
}

/// Wire format for an ordered Android CookieManager replacement.
///
/// Intentionally not `Debug`: this payload contains authentication cookies.
#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookieReplacePayload {
    url: String,
    cookies: Vec<String>,
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
                localstorage_export("p".into()),
                Err(Error::Unsupported)
            ));
            assert!(matches!(
                localstorage_import("p".into(), "eA==".into()),
                Err(Error::Unsupported)
            ));
            assert!(matches!(
                indexeddb_export("p".into()),
                Err(Error::Unsupported)
            ));
            assert!(matches!(
                indexeddb_import("p".into(), "eA==".into()),
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
        assert_eq!(CMD_LOCALSTORAGE_EXPORT, "localstorage_export");
        assert_eq!(CMD_LOCALSTORAGE_IMPORT, "localstorage_import");
        assert_eq!(CMD_INDEXEDDB_EXPORT, "indexeddb_export");
        assert_eq!(CMD_INDEXEDDB_IMPORT, "indexeddb_import");
        assert_eq!(CMD_WEBVIEW_COOKIES_GET, "webview_cookies_get");
        assert_eq!(CMD_WEBVIEW_COOKIES_REPLACE, "webview_cookies_replace");
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
            CMD_LOCALSTORAGE_EXPORT,
            CMD_LOCALSTORAGE_IMPORT,
            CMD_INDEXEDDB_EXPORT,
            CMD_INDEXEDDB_IMPORT,
            CMD_WEBVIEW_COOKIES_GET,
            CMD_WEBVIEW_COOKIES_REPLACE,
        ] {
            let marker = format!("fun {cmd}(");
            assert!(kt.contains(&marker), "Kotlin side is missing `fun {cmd}(`");
        }
        assert!(
            kt.contains("CookieManager.getInstance().getCookie(url) ?: \"\""),
            "an empty Android cookie jar must not return null across Kotlin"
        );
        assert!(
            kt.contains("manager.removeAllCookies")
                && kt.contains("manager.setCookie(url, cookieHeaders[index])"),
            "Android cookies must be cleared and restored sequentially before navigation"
        );
    }
}
