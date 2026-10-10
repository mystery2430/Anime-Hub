//! Where the AES-256 master key comes from, per platform.
//!
//! | Platform | Key source | Notes |
//! |---|---|---|
//! | Android | Android Keystore, used *inside* Kotlin | The raw key never enters the Rust process; [`crate::secure::keystore_android`] delegates seal/open to a `KeyStore`-backed cipher. |
//! | Windows | DPAPI `CryptProtectData` (user scope) | The random key is stored as a DPAPI blob in the app-data dir. |
//! | Linux | XDG Secret Service (gnome-keyring / KWallet) | Falls back to a `0600` key file only at first setup when no Secret Service exists. The choice is recorded in `key-source` and kept. A locked or failing keyring blocks opening; it never creates a second key. |
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

    pub(super) const KEY_FILE_NAME: &str = "animehub.key";

    // Linux uses `linux::load`; only macOS (and the tests below) need the plain file key.
    #[cfg(target_os = "macos")]
    pub fn load(dir: &Path) -> AppResult<MasterKey> {
        let k = file_fallback(dir)?;
        Ok(MasterKey {
            backend: Backend::EncryptedFile,
            key: Some(k),
        })
    }

    #[cfg(any(target_os = "macos", test))]
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
//
// The key source is recorded in `key-source` in the config dir. The record,
// not the keyring's current answer, decides which key may open the data:
//   * `secret-service`: the keyring key sealed the data. If the keyring is
//     locked, missing or failing, the app refuses to open. It never creates a
//     second key.
//   * `file`: the file key sealed the data. The keyring is ignored, even after
//     it recovers, and is never written to.
// ---------------------------------------------------------------------------
#[cfg(all(unix, not(target_os = "android"), not(target_os = "macos")))]
mod linux {
    use super::*;

    const SOURCE_FILE_NAME: &str = "key-source";
    const SOURCE_KEYRING: &str = "secret-service";
    const SOURCE_FILE: &str = "file";

    /// What the platform store reported, reduced to what the policy needs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum StoreError {
        /// No store is registered or reachable (no Secret Service running).
        Unavailable,
        /// The store exists but is locked, or the unlock was refused.
        Locked,
        /// Anything else: bad encoding, invalid, ambiguous, and so on.
        Other,
    }

    /// The narrow surface the policy needs. `Ok(None)` means the store answered
    /// and has no entry.
    pub(super) trait SecretStore {
        fn get(&self) -> Result<Option<String>, StoreError>;
        fn set(&self, value: &str) -> Result<(), StoreError>;
    }

    fn classify(e: &keyring::Error) -> StoreError {
        match e {
            keyring::Error::NoDefaultStore | keyring::Error::PlatformFailure(_) => {
                StoreError::Unavailable
            }
            keyring::Error::NoStorageAccess(_) => StoreError::Locked,
            _ => StoreError::Other,
        }
    }

    /// The real Secret Service through the `keyring` crate. `Entry::new` is the
    /// call that connects, so its failure is kept and reported on every use.
    struct KeyringStore(Result<keyring::Entry, StoreError>);

    impl KeyringStore {
        fn open() -> Self {
            Self(keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| classify(&e)))
        }
    }

    impl SecretStore for KeyringStore {
        fn get(&self) -> Result<Option<String>, StoreError> {
            let entry = self.0.as_ref().map_err(|e| *e)?;
            match entry.get_password() {
                Ok(v) => Ok(Some(v)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(classify(&e)),
            }
        }

        fn set(&self, value: &str) -> Result<(), StoreError> {
            let entry = self.0.as_ref().map_err(|e| *e)?;
            entry.set_password(value).map_err(|e| classify(&e))
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Source {
        Keyring,
        File,
    }

    pub fn load(dir: &Path) -> AppResult<MasterKey> {
        resolve(&KeyringStore::open(), dir)
    }

    /// Pick the key for `dir`. Pure policy over a store and a directory, so the
    /// tests can drive it with an in-memory store.
    pub(super) fn resolve(store: &dyn SecretStore, dir: &Path) -> AppResult<MasterKey> {
        match read_source(dir)? {
            Some(Source::File) => {
                let k = unix_file::existing_key(dir)?.ok_or_else(|| {
                    AppError::Keyring(
                        "anahtar dosyası bulunamadı; yeni anahtar oluşturulmadı, veriler açılmadı."
                            .into(),
                    )
                })?;
                Ok(file_key(k))
            }
            Some(Source::Keyring) => match store.get() {
                Ok(Some(hex)) => Ok(keyring_key(unix_file::decode_hex_key(&hex)?)),
                Ok(None) => Err(keyring_entry_missing()),
                Err(_) => Err(keyring_unavailable()),
            },
            None => resolve_unrecorded(store, dir),
        }
    }

    /// No record yet: a first start, or an install from before the record
    /// existed.
    fn resolve_unrecorded(store: &dyn SecretStore, dir: &Path) -> AppResult<MasterKey> {
        // A key file that predates the record may be the key that sealed the
        // data. Keep it, even when a keyring key exists as well.
        if let Some(k) = unix_file::existing_key(dir)? {
            write_source(dir, Source::File)?;
            return Ok(file_key(k));
        }

        // Encrypted documents with no record and no key file were sealed by a
        // key we cannot name. Only a keyring key can be that key.
        let has_data = sealed_data_present(dir)?;
        match store.get() {
            Ok(Some(hex)) => {
                let k = unix_file::decode_hex_key(&hex)?;
                write_source(dir, Source::Keyring)?;
                Ok(keyring_key(k))
            }
            Ok(None) if has_data => Err(keyring_entry_missing()),
            Ok(None) => {
                let key = Zeroizing::new(generate_key());
                match store.set(&hex::encode(*key)) {
                    Ok(()) => {
                        write_source(dir, Source::Keyring)?;
                        Ok(keyring_key(key))
                    }
                    // No Secret Service at all: nothing can hold a keyring key.
                    Err(StoreError::Unavailable) => create_file_key(dir),
                    Err(_) => Err(keyring_unavailable()),
                }
            }
            // Nothing sealed yet and no Secret Service running: first setup
            // without a keyring. Record the file key so it is never replaced.
            Err(StoreError::Unavailable) if !has_data => create_file_key(dir),
            // Locked or failing keyring, and we cannot tell whether it holds
            // the key for existing data. Create nothing.
            Err(_) => Err(keyring_unavailable()),
        }
    }

    fn create_file_key(dir: &Path) -> AppResult<MasterKey> {
        let k = unix_file::create_key(dir)?;
        write_source(dir, Source::File)?;
        Ok(file_key(k))
    }

    fn file_key(k: Zeroizing<[u8; 32]>) -> MasterKey {
        MasterKey {
            backend: Backend::EncryptedFile,
            key: Some(k),
        }
    }

    fn keyring_key(k: Zeroizing<[u8; 32]>) -> MasterKey {
        MasterKey {
            backend: Backend::SecretService,
            key: Some(k),
        }
    }

    fn keyring_unavailable() -> AppError {
        AppError::Keyring(
            "anahtar deposu kilitli veya geçici olarak kullanılamıyor; veriler açılmadı. \
             Depoyu açıp uygulamayı yeniden başlatın."
                .into(),
        )
    }

    fn keyring_entry_missing() -> AppError {
        AppError::Keyring(
            "anahtar deposunda kayıtlı anahtar bulunamadı; yeni anahtar oluşturulmadı, \
             veriler açılmadı."
                .into(),
        )
    }

    fn read_source(dir: &Path) -> AppResult<Option<Source>> {
        match std::fs::read_to_string(dir.join(SOURCE_FILE_NAME)) {
            Ok(s) => match s.trim() {
                SOURCE_KEYRING => Ok(Some(Source::Keyring)),
                SOURCE_FILE => Ok(Some(Source::File)),
                _ => Err(AppError::Keyring(
                    "anahtar kaynağı kaydı tanınmıyor; veriler açılmadı.".into(),
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write_source(dir: &Path, source: Source) -> AppResult<()> {
        std::fs::create_dir_all(dir)?;
        let name = match source {
            Source::Keyring => SOURCE_KEYRING,
            Source::File => SOURCE_FILE,
        };
        let tmp = dir.join(format!("{SOURCE_FILE_NAME}.tmp"));
        std::fs::write(&tmp, name)?;
        std::fs::rename(&tmp, dir.join(SOURCE_FILE_NAME))?;
        Ok(())
    }

    /// Whether any encrypted document is already on disk (`registry.bin`,
    /// `secret.*.bin`).
    fn sealed_data_present(dir: &Path) -> AppResult<bool> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            if entry?.file_name().to_string_lossy().ends_with(".bin") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::{Cell, RefCell};

        /// In-memory store. While `outage` is set, every call fails with it.
        struct FakeStore {
            value: RefCell<Option<String>>,
            outage: Cell<Option<StoreError>>,
            sets: Cell<u32>,
        }

        impl FakeStore {
            fn with(value: Option<&str>) -> Self {
                Self {
                    value: RefCell::new(value.map(str::to_string)),
                    outage: Cell::new(None),
                    sets: Cell::new(0),
                }
            }
            fn down(&self, e: Option<StoreError>) {
                self.outage.set(e);
            }
        }

        impl SecretStore for FakeStore {
            fn get(&self) -> Result<Option<String>, StoreError> {
                if let Some(e) = self.outage.get() {
                    return Err(e);
                }
                Ok(self.value.borrow().clone())
            }
            fn set(&self, value: &str) -> Result<(), StoreError> {
                if let Some(e) = self.outage.get() {
                    return Err(e);
                }
                self.sets.set(self.sets.get() + 1);
                *self.value.borrow_mut() = Some(value.to_string());
                Ok(())
            }
        }

        fn open(store: &dyn SecretStore, dir: &Path) -> MasterKey {
            resolve(store, dir).expect("key should resolve")
        }

        fn key_bytes(m: &MasterKey) -> [u8; 32] {
            *m.raw().expect("key bytes")
        }

        fn source_of(dir: &Path) -> String {
            std::fs::read_to_string(dir.join(SOURCE_FILE_NAME)).unwrap()
        }

        #[test]
        fn first_setup_without_secret_service_uses_and_records_a_file_key() {
            let dir = tempfile::tempdir().unwrap();
            let store = FakeStore::with(None);
            store.down(Some(StoreError::Unavailable));

            let m = open(&store, dir.path());
            assert_eq!(m.backend(), Backend::EncryptedFile);
            assert!(dir.path().join(unix_file::KEY_FILE_NAME).exists());
            assert_eq!(source_of(dir.path()), "file");
            assert_eq!(store.sets.get(), 0, "no keyring entry may be written");
        }

        #[test]
        fn keyring_key_survives_a_reopen() {
            let dir = tempfile::tempdir().unwrap();
            let store = FakeStore::with(None);

            let first = key_bytes(&open(&store, dir.path()));
            assert_eq!(store.sets.get(), 1);
            assert!(!dir.path().join(unix_file::KEY_FILE_NAME).exists());
            assert_eq!(source_of(dir.path()), "secret-service");

            let again = open(&store, dir.path());
            assert_eq!(again.backend(), Backend::SecretService);
            assert_eq!(key_bytes(&again), first, "reopen must return the same key");
            assert_eq!(store.sets.get(), 1, "reopen must not write a new key");
        }

        #[test]
        fn temporary_keyring_failure_never_creates_a_second_key() {
            let dir = tempfile::tempdir().unwrap();
            let store = FakeStore::with(None);
            open(&store, dir.path());

            for e in [
                StoreError::Locked,
                StoreError::Unavailable,
                StoreError::Other,
            ] {
                store.down(Some(e));
                assert!(
                    resolve(&store, dir.path()).is_err(),
                    "{e:?} must fail closed"
                );
                assert!(!dir.path().join(unix_file::KEY_FILE_NAME).exists());
                assert_eq!(source_of(dir.path()), "secret-service", "record unchanged");
            }
            assert_eq!(store.sets.get(), 1, "a failing keyring must not be written");
        }

        #[test]
        fn locked_keyring_on_first_setup_creates_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let store = FakeStore::with(None);
            store.down(Some(StoreError::Locked));

            assert!(resolve(&store, dir.path()).is_err());
            assert!(!dir.path().join(unix_file::KEY_FILE_NAME).exists());
            assert!(!dir.path().join(SOURCE_FILE_NAME).exists());
            assert_eq!(store.sets.get(), 0);
        }

        #[test]
        fn keyring_recovered_later_does_not_replace_the_file_key() {
            let dir = tempfile::tempdir().unwrap();
            let offline = FakeStore::with(None);
            offline.down(Some(StoreError::Unavailable));
            let file_key = key_bytes(&open(&offline, dir.path()));

            // The keyring is back and holds some other key.
            let back = FakeStore::with(Some(hex::encode([7u8; 32]).as_str()));
            let m = open(&back, dir.path());
            assert_eq!(m.backend(), Backend::EncryptedFile);
            assert_eq!(key_bytes(&m), file_key);
            assert_eq!(
                back.sets.get(),
                0,
                "the recovered keyring must not be written"
            );
        }

        #[test]
        fn fallback_never_opens_or_overwrites_data_sealed_under_another_key() {
            let dir = tempfile::tempdir().unwrap();
            let sealed = b"sealed under the keyring key".to_vec();
            std::fs::write(dir.path().join("registry.bin"), &sealed).unwrap();

            // Data exists, no record, no key file: a locked or missing keyring
            // must not produce a key, and the document must stay byte-identical.
            for e in [StoreError::Unavailable, StoreError::Locked] {
                let store = FakeStore::with(None);
                store.down(Some(e));
                assert!(resolve(&store, dir.path()).is_err());
                assert!(!dir.path().join(unix_file::KEY_FILE_NAME).exists());
            }
            let empty = FakeStore::with(None);
            assert!(
                resolve(&empty, dir.path()).is_err(),
                "no keyring key for existing data"
            );
            assert_eq!(empty.sets.get(), 0);
            assert_eq!(
                std::fs::read(dir.path().join("registry.bin")).unwrap(),
                sealed
            );
        }

        #[test]
        fn existing_data_with_a_keyring_key_adopts_that_key() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("registry.bin"), b"x").unwrap();
            let store = FakeStore::with(Some(hex::encode([9u8; 32]).as_str()));

            let m = open(&store, dir.path());
            assert_eq!(m.backend(), Backend::SecretService);
            assert_eq!(key_bytes(&m), [9u8; 32]);
            assert_eq!(source_of(dir.path()), "secret-service");
            assert_eq!(store.sets.get(), 0);
        }

        #[test]
        fn recorded_keyring_key_missing_from_the_store_fails_closed() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SOURCE_FILE_NAME), "secret-service").unwrap();
            let store = FakeStore::with(None);

            assert!(resolve(&store, dir.path()).is_err());
            assert_eq!(
                store.sets.get(),
                0,
                "a missing entry must not be re-created"
            );
            assert!(!dir.path().join(unix_file::KEY_FILE_NAME).exists());
        }

        #[test]
        fn existing_key_file_without_a_record_is_kept_over_a_keyring_key() {
            let dir = tempfile::tempdir().unwrap();
            let file_key = unix_file::create_key(dir.path()).unwrap();
            let store = FakeStore::with(Some(hex::encode([5u8; 32]).as_str()));

            let m = open(&store, dir.path());
            assert_eq!(m.backend(), Backend::EncryptedFile);
            assert_eq!(key_bytes(&m), *file_key);
            assert_eq!(source_of(dir.path()), "file");
            assert_eq!(store.sets.get(), 0);
        }

        #[test]
        fn unknown_source_record_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SOURCE_FILE_NAME), "something-else").unwrap();
            let store = FakeStore::with(None);
            assert!(resolve(&store, dir.path()).is_err());
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
