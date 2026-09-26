//! Android-side bridges for AnimeHub.
//!
//! Two platform features have no Rust API and must go through the JVM:
//!
//! * **AndroidKeyStore** — an AES/GCM key that never leaves the secure
//!   hardware. Rust hands over plaintext and gets ciphertext back, so the raw
//!   key is never present in this process.
//! * **Picture-in-Picture** — `PictureInPictureParams` /
//!   `enterPictureInPictureMode()` live on the host `Activity`.
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

// ---------------------------------------------------------------------------
// Mobile command names
// ---------------------------------------------------------------------------

pub const CMD_KEYSTORE_SEAL: &str = "keystore_seal";
pub const CMD_KEYSTORE_OPEN: &str = "keystore_open";
pub const CMD_ENTER_PIP: &str = "enter_pip";
pub const CMD_SET_PIP_AUTO_ENTER: &str = "set_pip_auto_enter";

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
            KeystorePayload {
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
            KeystorePayload {
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

/// Enter Picture-in-Picture with the given aspect ratio.
pub fn enter_pip(aspect_num: u32, aspect_den: u32) -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        call_plugin(
            CMD_ENTER_PIP,
            PipPayload {
                num: aspect_num,
                den: aspect_den,
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (aspect_num, aspect_den);
        Err(Error::Unsupported)
    }
}

/// Enable or disable auto-enter PiP (Android 12 / API 31+).
pub fn set_pip_auto_enter(enabled: bool) -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        call_plugin(CMD_SET_PIP_AUTO_ENTER, AutoEnterPayload { enabled })
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

/// Wire format for the Keystore commands.
#[cfg(target_os = "android")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KeystorePayload {
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
/// deserialises the `JSObject` the Kotlin side resolves with.
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
        }
    }

    #[test]
    fn keystore_result_deserialises_from_kotlin_camelcase() {
        let json = r#"{"value":"SGVsbG8="}"#;
        let r: KeystoreResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.value, "SGVsbG8=");
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
        assert_eq!(PLUGIN_ALIAS, "animehub-android");
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
}
