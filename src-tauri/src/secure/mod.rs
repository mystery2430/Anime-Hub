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
        std::fs::create_dir_all(dir)?;
        let path = dir.join(Self::KEY_FILE);

        if path.exists() {
            let wrapped = std::fs::read_to_string(&path)?;
            let raw = crate::android_bridge::keystore_open(Self::KEY_PURPOSE, &wrapped)?;
            if raw.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&raw);
                return Ok(k);
            }
            // Wrong length => unusable; regenerate rather than fail forever.
            log::warn!("wrapped key had unexpected length; regenerating");
        }

        let key = crypto::generate_key();
        let wrapped = crate::android_bridge::keystore_seal(Self::KEY_PURPOSE, &key)?;
        std::fs::write(&path, wrapped)?;
        Ok(key)
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
    fn write_wrapped(&self, file_name: &str, plaintext: &[u8], aad: &[u8]) -> AppResult<()> {
        std::fs::create_dir_all(&self.dir)?;
        let envelope = self.sealer_for(file_name).seal(plaintext, aad)?;
        let wrapped = crate::android_bridge::keystore_seal(file_name, &envelope)?;
        let target = self.path_for(file_name);
        let tmp = target.with_extension("tmp");
        std::fs::write(&tmp, wrapped)?;
        std::fs::rename(&tmp, target)?;
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
        match std::fs::remove_file(self.path_for(&store::secret_file_name(name))) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn wipe_secrets(&self) -> AppResult<usize> {
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
}
