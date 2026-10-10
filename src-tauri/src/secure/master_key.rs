//! Where the AES-256 master key comes from, per platform.
//!
//! | Platform | Key source | Notes |
//! |---|---|---|
//! | Android | Android Keystore, used *inside* Kotlin | The raw key never enters the Rust process; [`crate::secure::keystore_android`] delegates seal/open to a `KeyStore`-backed cipher. |
//! | Windows | DPAPI `CryptProtectData` (user scope) | The random key is stored as a DPAPI blob in the app-data dir. |
//! | Linux | XDG Secret Service (gnome-keyring / KWallet) | Falls back to a `0600` key file only when no keyring is running. |
//!
//! The Linux fallback is a deliberate, documented degradation: a headless or
//! minimal session often has no Secret Service, and refusing to start would be
//! worse than storing the key with restrictive permissions. The active backend
//! is reported to the About screen so the user can see what they got.

use crate::error::{AppError, AppResult};
// Android never materialises a Rust key. Every other host does: Windows via
// DPAPI, Linux via Secret Service or a file, macOS via the file only.
#[cfg(not(target_os = "android"))]
use crate::secure::crypto::generate_key;
use std::path::Path;
#[cfg(all(unix, not(target_os = "android")))]
use std::path::PathBuf;
use zeroize::Zeroizing;

pub const KEYRING_SERVICE: &str = "dev.animehub.app";
pub const KEYRING_USER: &str = "master-key";

/// Human-readable name of the active backend, shown in About.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    AndroidKeystore,
    WindowsDpapi,
    SecretService,
    EncryptedFile,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::AndroidKeystore => "Android Keystore",
            Backend::WindowsDpapi => "Windows DPAPI",
            Backend::SecretService => "Secret Service (gnome-keyring / KWallet)",
            Backend::EncryptedFile => "Şifreli anahtar dosyası (yedek)",
        }
    }

    /// `true` when the key is protected by a real OS secret store rather than
    /// by file permissions alone.
    pub fn is_os_backed(self) -> bool {
        !matches!(self, Backend::EncryptedFile)
    }
}

/// Holds the master key for the process lifetime.
///
/// On Windows/Linux the key is materialised in memory (that is unavoidable if
/// Rust is doing the AES work). On Android the "key" is a handle: the actual
/// cipher operations happen in Kotlin, so [`Sealer::on_android`] is `None`.
pub struct MasterKey {
    backend: Backend,
    key: Option<Zeroizing<[u8; 32]>>,
}

impl MasterKey {
    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn raw(&self) -> AppResult<&[u8; 32]> {
        // `Zeroizing<[u8; 32]>` derefs to the array; hand out the plain
        // reference so callers never have to know about the wrapper.
        self.key.as_deref().ok_or_else(|| {
            AppError::Keyring("bu platformda anahtar Rust tarafında tutulmuyor".into())
        })
    }

    /// Android: no Rust-side key at all.
    pub fn android() -> Self {
        MasterKey {
            backend: Backend::AndroidKeystore,
            key: None,
        }
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        // `Zeroizing` already wipes the array; this is belt-and-braces so the
        // intent is obvious to a reader.
        self.key = None;
    }
}

/// Load (creating on first run) the master key for this platform.
///
/// `dir` is the app config directory from `tauri::Manager::path()`.
pub fn load_master_key(dir: &Path) -> AppResult<MasterKey> {
    #[cfg(target_os = "android")]
    {
        let _ = dir;
        Ok(MasterKey::android())
    }

    #[cfg(windows)]
    {
        windows::load(dir)
    }

    #[cfg(all(unix, not(target_os = "android"), not(target_os = "macos")))]
    {
        linux::load(dir)
    }

    #[cfg(target_os = "macos")]
    {
        // Not a shipping target. CI still runs `cargo test` here, and those
        // tests load a real key, so use the same owner-only file as the Linux
        // fallback rather than failing the suite.
        unix_file::load(dir)
    }
}

// ---------------------------------------------------------------------------
// Unix file fallback. Linux uses this when Secret Service is absent. macOS
// has no shipping keystore; CI still needs a real key for `cargo test`.
// ---------------------------------------------------------------------------
#[cfg(all(unix, not(target_os = "android")))]
mod unix_file {
    use super::*;

    const KEY_FILE_NAME: &str = "animehub.key";

    pub fn load(dir: &Path) -> AppResult<MasterKey> {
        let k = file_fallback(dir)?;
        Ok(MasterKey {
            backend: Backend::EncryptedFile,
            key: Some(k),
        })
    }

    fn file_fallback(dir: &Path) -> AppResult<Zeroizing<[u8; 32]>> {
        match existing_key(dir)? {
            Some(k) => Ok(k),
            None => create_key(dir),
        }
    }

    /// The key in the key file, if there is one. Never creates the file.
    pub(super) fn existing_key(dir: &Path) -> AppResult<Option<Zeroizing<[u8; 32]>>> {
        let path = dir.join(KEY_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&path)?;
        decode_hex_key(&String::from_utf8_lossy(&raw)).map(Some)
    }

    /// Create a new key file and return the key stored in it. If another
    /// process created the file first, the key already on disk is returned.
    pub(super) fn create_key(dir: &Path) -> AppResult<Zeroizing<[u8; 32]>> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(KEY_FILE_NAME);
        let key = Zeroizing::new(generate_key());
        write_private(&path, hex::encode(*key).as_bytes())?;
        existing_key(dir)?.ok_or_else(|| AppError::Crypto("anahtar dosyası yazılamadı".into()))
    }

    /// Write with `0600`, creating the file exclusively so a pre-existing
    /// symlink or world-readable file is never silently reused.
    fn write_private(path: &PathBuf, bytes: &[u8]) -> AppResult<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let mut f = match opts.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Someone raced us; adopt whatever is there.
                let raw = std::fs::read(path)?;
                let _ = decode_hex_key(&String::from_utf8_lossy(&raw))?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        use std::io::Write as _;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    }

    pub(super) fn decode_hex_key(s: &str) -> AppResult<Zeroizing<[u8; 32]>> {
        let bytes = hex::decode(s.trim()).map_err(|_| AppError::Crypto("anahtar bozuk".into()))?;
        if bytes.len() != 32 {
            return Err(AppError::Crypto("anahtar uzunluğu yanlış".into()));
        }
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&bytes);
        Ok(key)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;

        #[test]
        fn hex_key_roundtrips() {
            let k = generate_key();
            let back = decode_hex_key(&hex::encode(k)).unwrap();
            assert_eq!(&*back, &k);
        }

        #[test]
        fn malformed_hex_key_is_rejected() {
            assert!(decode_hex_key("zz").is_err());
            assert!(decode_hex_key(&hex::encode([0u8; 16])).is_err());
        }

        #[test]
        fn fallback_creates_a_0600_key_file_and_reuses_it() {
            let dir = tempfile::tempdir().unwrap();
            let first = file_fallback(dir.path()).unwrap();
            let path = dir.path().join(KEY_FILE_NAME);
            assert!(path.exists());

            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be owner-only");

            let second = file_fallback(dir.path()).unwrap();
            assert_eq!(&*first, &*second, "key must be stable across loads");
        }

        #[test]
        fn fallback_writes_exclusively() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(KEY_FILE_NAME);
            std::fs::write(&path, hex::encode(generate_key())).unwrap();
            // Must adopt the existing file instead of erroring.
            assert!(write_private(&path, b"ignored").is_ok());
            assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 64);
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: XDG Secret Service, with the encrypted-file fallback above.
// ---------------------------------------------------------------------------
#[cfg(all(unix, not(target_os = "android"), not(target_os = "macos")))]
mod linux {
    use super::*;

    pub fn load(dir: &Path) -> AppResult<MasterKey> {
        if let Some(k) = try_secret_service()? {
            return Ok(MasterKey {
                backend: Backend::SecretService,
                key: Some(k),
            });
        }
        unix_file::load(dir)
    }

    fn try_secret_service() -> AppResult<Option<Zeroizing<[u8; 32]>>> {
        let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            Ok(e) => e,
            // No Secret Service running (headless, minimal session, …).
            Err(_) => return Ok(None),
        };

        match entry.get_password() {
            Ok(hexkey) => Ok(Some(unix_file::decode_hex_key(&hexkey)?)),
            Err(keyring::Error::NoEntry) => {
                let key = generate_key();
                let hex = hex::encode(key);
                entry
                    .set_password(&hex)
                    .map_err(|e| AppError::Keyring(format!("anahtar kaydedilemedi: {e}")))?;
                Ok(Some(Zeroizing::new(key)))
            }
            // Locked keyring, D-Bus failure, etc. — degrade rather than die.
            Err(_) => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: DPAPI.
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod windows {
    use super::*;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_PROMPTSTRUCT, CRYPTPROTECT_UI_FORBIDDEN,
        CRYPT_INTEGER_BLOB,
    };

    const DPAPI_FILE_NAME: &str = "animehub.key.dpapi";
    const ENTROPY: &[u8] = b"animehub/master-key/v1";

    pub fn load(dir: &Path) -> AppResult<MasterKey> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(DPAPI_FILE_NAME);

        if path.exists() {
            let blob = std::fs::read(&path)?;
            let key = dpapi_unprotect(&blob)?;
            return Ok(MasterKey {
                backend: Backend::WindowsDpapi,
                key: Some(key),
            });
        }

        let key = generate_key();
        let blob = dpapi_protect(&key)?;
        std::fs::write(&path, blob)?;
        Ok(MasterKey {
            backend: Backend::WindowsDpapi,
            key: Some(Zeroizing::new(key)),
        })
    }

    /// DPAPI wants a mutable byte pointer. The slice is not written.
    fn blob_from(bytes: &[u8]) -> AppResult<CRYPT_INTEGER_BLOB> {
        let cb_data = u32::try_from(bytes.len())
            .map_err(|_| AppError::Keyring("veri DPAPI için çok büyük".into()))?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: cb_data,
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    fn dpapi_protect(plain: &[u8]) -> AppResult<Vec<u8>> {
        let data_in = blob_from(plain)?;
        let entropy = blob_from(ENTROPY)?;
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: core::ptr::null_mut(),
        };

        // windows-sys takes `*const` for the inputs and the unused prompt.
        // Untyped `null_mut()` does not infer those parameter types.
        let ok = unsafe {
            CryptProtectData(
                &data_in,
                core::ptr::null(),
                &entropy,
                core::ptr::null::<core::ffi::c_void>(),
                core::ptr::null::<CRYPTPROTECT_PROMPTSTRUCT>(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(AppError::Keyring("DPAPI şifrelemesi başarısız".into()));
        }
        // `std` has no `From<u32> for usize` (not lossless on 16-bit), so a
        // plain widening cast is the way; `cbData` is a `u32` byte count.
        let blob = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            let _freed = LocalFree(out.pbData.cast());
        }
        Ok(blob)
    }

    fn dpapi_unprotect(blob: &[u8]) -> AppResult<Zeroizing<[u8; 32]>> {
        let data_in = blob_from(blob)?;
        let entropy = blob_from(ENTROPY)?;
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: core::ptr::null_mut(),
        };

        let ok = unsafe {
            CryptUnprotectData(
                &data_in,
                core::ptr::null_mut(),
                &entropy,
                core::ptr::null::<core::ffi::c_void>(),
                core::ptr::null::<CRYPTPROTECT_PROMPTSTRUCT>(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(AppError::Keyring(
                "DPAPI çözülemedi — farklı bir Windows kullanıcısı olabilir".into(),
            ));
        }
        let plain = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            let _freed = LocalFree(out.pbData.cast());
        }
        let plain = Zeroizing::new(plain);
        if plain.len() != 32 {
            return Err(AppError::Crypto("anahtar uzunluğu yanlış".into()));
        }
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&plain);
        Ok(key)
    }
}
