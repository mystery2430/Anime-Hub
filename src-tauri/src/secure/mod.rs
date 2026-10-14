//! Encrypted storage: the envelope format, the platform key source, and the
//! document store built on top of both.

pub mod crypto;
pub mod master_key;
pub mod store;

use crate::error::AppResult;
use crate::secure::master_key::Backend;
use crate::sites::registry::Registry;
use zeroize::Zeroizing;

/// Everything the app persists goes through this trait, so the desktop and
/// Android key backends are interchangeable at the call sites.
///
/// The required methods work on raw bytes: that is what keeps the trait
/// object-safe (`dyn SecretProvider`), which the app state needs in order to
/// hold either backend behind one `Box`. The generic JSON helpers below are
/// provided methods layered on top.
pub trait SecretProvider: Send + Sync {
    fn backend(&self) -> Backend;

    fn load_doc_bytes(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>>;
    fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()>;

    fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()>;
    fn get_secret(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>>;
    fn delete_secret(&self, name: &str) -> AppResult<()>;
    fn wipe_secrets(&self) -> AppResult<usize>;
    fn delete_site_session(&self, site_id: &str) -> AppResult<()>;

    // -- provided JSON helpers -------------------------------------------
    //
    // `where Self: Sized` keeps these generic methods out of the vtable,
    // which is what makes `dyn SecretProvider` legal: they are conveniences
    // over the byte-level methods above, never dispatched dynamically.

    fn load_doc<T: serde::de::DeserializeOwned>(&self, name: &str) -> AppResult<Option<T>>
    where
        Self: Sized,
    {
        match self.load_doc_bytes(name)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        }
    }

    fn save_doc<T: serde::Serialize>(&self, name: &str, value: &T) -> AppResult<()>
    where
        Self: Sized,
    {
        let bytes = serde_json::to_vec(value)?;
        self.save_doc_bytes(name, bytes.as_slice())
    }

    /// Load the registry, recovering to shipped defaults if it is missing or
    /// unreadable. A corrupt registry must never brick the launcher.
    /// The second element is a warning to surface in the UI.
    fn load_registry(&self) -> (Registry, Option<String>) {
        let decoded = match self.load_doc_bytes("registry.bin") {
            Ok(Some(bytes)) => serde_json::from_slice::<Registry>(&bytes)
                .map(Some)
                .map_err(crate::error::AppError::from),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        match decoded {
            Ok(Some(mut reg)) => {
                reg.ensure_builtins();
                (reg, None)
            }
            Ok(None) => (Registry::new_default(), None),
            Err(e) => (Registry::new_default(), Some(e.to_string())),
        }
    }

    fn save_registry(&self, reg: &Registry) -> AppResult<()> {
        let bytes = serde_json::to_vec(reg)?;
        self.save_doc_bytes("registry.bin", bytes.as_slice())
    }
}

impl SecretProvider for store::SecureStore {
    fn backend(&self) -> Backend {
        self.backend()
    }

    fn load_doc_bytes(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
        self.load_doc_bytes(name)
    }

    fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
        self.save_doc_bytes(name, bytes)
    }

    fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
        self.put_secret(name, plaintext)
    }

    fn get_secret(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
        self.get_secret(name)
    }

    fn delete_secret(&self, name: &str) -> AppResult<()> {
        self.delete_secret(name)
    }

    fn wipe_secrets(&self) -> AppResult<usize> {
        self.wipe_secrets()
    }

    fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
        store::SecureStore::delete_site_session(self, site_id)
    }
}

// ---------------------------------------------------------------------------
// Android: same envelope format, additionally wrapped by the Keystore.
// ---------------------------------------------------------------------------

/// Android store.
///
/// A random per-install key is generated once and kept *wrapped* by an
/// `AndroidKeyStore` key (AES/GCM, hardware-backed where available). The
/// wrapped blob is the only thing on disk, so the unwrapped key never touches
/// storage. Rust derives per-purpose subkeys from it for the actual document
/// encryption.
#[cfg(target_os = "android")]
pub struct AndroidStore {
    dir: std::path::PathBuf,
    registry_sealer: crypto::Sealer,
    secret_sealer: crypto::Sealer,
}

#[cfg(target_os = "android")]
impl AndroidStore {
    const KEY_PURPOSE: &'static str = "animehub-doc-key";
    const KEY_FILE: &'static str = "animehub.key.wrapped";

    pub fn new(dir: &std::path::Path) -> AppResult<Self> {
        let key = zeroize::Zeroizing::new(Self::load_or_create_key(dir)?);
        Ok(AndroidStore {
            dir: dir.to_path_buf(),
            registry_sealer: crypto::Sealer::new(&crypto::derive_subkey(&key, "registry/v1")),
            secret_sealer: crypto::Sealer::new(&crypto::derive_subkey(&key, "secrets/v1")),
        })
    }

    fn load_or_create_key(dir: &std::path::Path) -> AppResult<[u8; 32]> {
        // Hold the shared store lock for the entire check+create so two
        // concurrent callers cannot both see "no key yet" and create
        // divergent keys, and so a concurrent secret write/delete cannot
        // interleave with the key file creation. This makes the
        // check-then-create atomic with respect to `STORE_LOCK`.
        let _guard = crate::secure::store::STORE_LOCK
            .lock()
            .map_err(|_| crate::error::AppError::Storage("kilit alınamadı".into()))?;
        std::fs::create_dir_all(dir)?;
        let path = dir.join(Self::KEY_FILE);

        if path.exists() {
            let wrapped = std::fs::read_to_string(&path)?;
            // Empty file is indistinguishable from a failed previous write: treat
            // as corrupt rather than as \"no key yet\" so we do not silently
            // replace a key that sealed existing data.
            if wrapped.trim().is_empty() {
                let has_data = Self::sealed_data_present(dir)?;
                if has_data {
                    return Err(crate::error::AppError::Keyring(
                        "sarmalanmış anahtar dosyası bozuk; yeni anahtar oluşturulmadı, veriler açılmadı.".into(),
                    ));
                }
                log::warn!("wrapped key file empty with no sealed data; regenerating");
            } else {
                match crate::android_bridge::keystore_open(Self::KEY_PURPOSE, &wrapped) {
                    Ok(raw) if raw.len() == 32 => {
                        let mut k = [0u8; 32];
                        k.copy_from_slice(&raw);
                        return Ok(k);
                    }
                    Ok(_) => {
                        let has_data = Self::sealed_data_present(dir)?;
                        if has_data {
                            return Err(crate::error::AppError::Keyring(
                                "sarmalanmış anahtar beklenmeyen uzunlukta; yeni anahtar oluşturulmadı, veriler açılmadı.".into(),
                            ));
                        }
                        log::warn!(
                            "wrapped key had unexpected length with no sealed data; regenerating"
                        );
                    }
                    Err(e) => {
                        // Keystore failures (locked, missing alias, etc.) must
                        // not silently create a second key that would orphan
                        // the existing encrypted documents.
                        let has_data = Self::sealed_data_present(dir)?;
                        if has_data {
                            return Err(crate::error::AppError::Keyring(format!(
                                "sarmalanmış anahtar açılamadı; yeni anahtar oluşturulmadı: {e}"
                            )));
                        }
                        return Err(crate::error::AppError::Keyring(format!(
                            "sarmalanmış anahtar açılamadı: {e}"
                        )));
                    }
                }
            }
        }

        let key = crypto::generate_key();
        let wrapped = crate::android_bridge::keystore_seal(Self::KEY_PURPOSE, &key)?;
        // The outer `STORE_LOCK` is already held, so this write is atomic
        // and durable without re-locking.
        {
            use std::io::Write as _;
            let unique = {
                use rand::RngCore as _;
                let mut b = [0u8; 8];
                rand::thread_rng().fill_bytes(&mut b);
                format!("{}-{}", std::process::id(), hex::encode(b))
            };
            let tmp = path.with_extension(format!("tmp.{unique}"));
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(wrapped.as_bytes())?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, &path)?;
            if let Ok(dir_file) = std::fs::File::open(dir) {
                let _ = dir_file.sync_all();
            }
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(key)
    }

    fn sealed_data_present(dir: &std::path::Path) -> AppResult<bool> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().to_string();
            if name.ends_with(".bin") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn aad(name: &str) -> Vec<u8> {
        let mut v = Vec::with_capacity(name.len() + 18);
        v.extend_from_slice(b"animehub/aad/v1/");
        v.extend_from_slice(name.as_bytes());
        v
    }

    fn path_for(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }

    fn sealer_for(&self, name: &str) -> &crypto::Sealer {
        if name == "registry.bin" {
            &self.registry_sealer
        } else {
            &self.secret_sealer
        }
    }

    /// Encrypt `plaintext`, wrap with the Keystore, and write atomically.
    /// The `STORE_LOCK` is held for the entire seal+wrap+write so a concurrent
    /// `delete_site_session` cannot delete the file after we have sealed but
    /// before we have written, which would otherwise resurrect the jar.
    fn write_wrapped(&self, file_name: &str, plaintext: &[u8], aad: &[u8]) -> AppResult<()> {
        let _guard = crate::secure::store::STORE_LOCK
            .lock()
            .map_err(|_| crate::error::AppError::Storage("kilit alınamadı".into()))?;
        std::fs::create_dir_all(&self.dir)?;
        let envelope = self.sealer_for(file_name).seal(plaintext, aad)?;
        let wrapped = crate::android_bridge::keystore_seal(file_name, &envelope)?;
        let target = self.path_for(file_name);
        let unique = {
            use rand::RngCore as _;
            let mut b = [0u8; 8];
            rand::thread_rng().fill_bytes(&mut b);
            format!("{}-{}", std::process::id(), hex::encode(b))
        };
        let tmp = target.with_extension(format!("tmp.{unique}"));
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(wrapped.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &target)?;
        if let Ok(dir_file) = std::fs::File::open(&self.dir) {
            let _ = dir_file.sync_all();
        }
        let _ = std::fs::remove_file(&tmp);
        Ok(())
    }

    fn read_wrapped(&self, file_name: &str, aad: &[u8]) -> AppResult<Option<Vec<u8>>> {
        let path = self.path_for(file_name);
        if !path.exists() {
            return Ok(None);
        }
        let wrapped = std::fs::read_to_string(&path)?;
        if wrapped.is_empty() {
            return Ok(None);
        }
        let envelope = crate::android_bridge::keystore_open(file_name, &wrapped)?;
        Ok(Some(self.sealer_for(file_name).open(&envelope, aad)?))
    }
}

#[cfg(target_os = "android")]
impl SecretProvider for AndroidStore {
    fn backend(&self) -> Backend {
        Backend::AndroidKeystore
    }

    fn load_doc_bytes(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
        Ok(self
            .read_wrapped(name, &Self::aad(name))?
            .map(Zeroizing::new))
    }

    fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
        self.write_wrapped(name, bytes, &Self::aad(name))
    }

    fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
        let file = store::secret_file_name(name);
        self.write_wrapped(&file, plaintext, &Self::aad(&format!("secret/{name}")))
    }

    fn get_secret(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
        let file = store::secret_file_name(name);
        Ok(self
            .read_wrapped(&file, &Self::aad(&format!("secret/{name}")))?
            .map(Zeroizing::new))
    }

    fn delete_secret(&self, name: &str) -> AppResult<()> {
        let _guard = crate::secure::store::STORE_LOCK
            .lock()
            .map_err(|_| crate::error::AppError::Storage("kilit alınamadı".into()))?;
        match std::fs::remove_file(self.path_for(&store::secret_file_name(name))) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn wipe_secrets(&self) -> AppResult<usize> {
        let _guard = crate::secure::store::STORE_LOCK
            .lock()
            .map_err(|_| crate::error::AppError::Storage("kilit alınamadı".into()))?;
        let mut n = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("secret.") {
                std::fs::remove_file(entry.path())?;
                n += 1;
            }
        }
        Ok(n)
    }

    fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
        let _guard = crate::secure::store::STORE_LOCK
            .lock()
            .map_err(|_| crate::error::AppError::Storage("kilit alınamadı".into()))?;
        for kind in ["cookies", "localstorage", "indexeddb"] {
            let name = format!("{kind}-{site_id}");
            let path = self.path_for(&store::secret_file_name(&name));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}
