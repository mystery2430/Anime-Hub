//! Android-specific bridges.
//!
//! Both are implemented in Kotlin (see `src-tauri/android-plugin/`) because the
//! platform APIs involved — `AndroidKeyStore` and
//! `PictureInPictureParams` — are only reachable from the JVM side.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

/// Result of a Keystore encrypt/decrypt round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeystoreResult {
    pub value: String,
}

/// Encrypt `plaintext` with the Android Keystore-backed key.
///
/// The raw key never leaves the secure hardware / TEE; Rust only ever sees
/// ciphertext. `purpose` is bound into the alias so two stores cannot swap
/// blobs.
#[cfg(target_os = "android")]
pub fn keystore_seal(purpose: &str, plaintext: &[u8]) -> AppResult<String> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(plaintext);
    let r: KeystoreResult = animehub_android::keystore_seal(purpose.into(), b64)
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    Ok(r.value)
}

/// Decrypt a blob produced by [`keystore_seal`].
#[cfg(target_os = "android")]
pub fn keystore_open(purpose: &str, sealed: &str) -> AppResult<Vec<u8>> {
    use base64::Engine as _;
    let r: KeystoreResult = animehub_android::keystore_open(purpose.into(), sealed.to_string())
        .map_err(|e| AppError::Keyring(e.to_string()))?;
    base64::engine::general_purpose::STANDARD
        .decode(r.value)
        .map_err(|_| AppError::Crypto("base64 çözülemedi".into()))
}

/// Stub for non-Android builds so call sites stay uniform.
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

/// Enter Picture-in-Picture mode.
///
/// On Android this calls `enterPictureInPictureMode()` on the host Activity.
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

/// Ask the Activity to enable auto-enter PiP when the user leaves the app.
///
/// Requires Android 12 (API 31)+; returns `false` on older devices.
#[cfg(target_os = "android")]
pub fn set_pip_auto_enter(enabled: bool) -> AppResult<bool> {
    animehub_android::set_pip_auto_enter(enabled).map_err(|e| AppError::Other(e.to_string()))
}

#[cfg(not(target_os = "android"))]
pub fn set_pip_auto_enter(_enabled: bool) -> AppResult<bool> {
    Ok(false)
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
    }
}
