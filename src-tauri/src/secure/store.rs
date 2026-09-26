//! Encrypted-at-rest document store.
//!
//! Two things live here: the site [`Registry`] and opaque secret blobs (the
//! AniList access token). Both are AES-GCM envelopes; the AAD binds each
//! document to its filename so a blob cannot be swapped between stores.

use crate::error::{AppError, AppResult};
use crate::secure::crypto::{derive_subkey, Sealer};
use crate::secure::master_key::{Backend, MasterKey};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub struct SecureStore {
    dir: PathBuf,
    registry_sealer: Sealer,
    secret_sealer: Sealer,
    backend: Backend,
}

impl SecureStore {
    /// Build a store from the platform master key.
    ///
    /// Returns `None` on Android, where sealing happens in Kotlin; the Android
    /// path goes through [`AndroidSecureStore`] instead.
    pub fn new(key: &MasterKey, dir: &Path) -> AppResult<Self> {
        let master = key.raw()?;
        Ok(SecureStore {
            dir: dir.to_path_buf(),
            registry_sealer: Sealer::new(&derive_subkey(master, "registry/v1")),
            secret_sealer: Sealer::new(&derive_subkey(master, "secrets/v1")),
            backend: key.backend(),
        })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    fn aad(name: &str) -> Vec<u8> {
        let mut v = Vec::with_capacity(name.len() + 18);
        v.extend_from_slice(b"animehub/aad/v1/");
        v.extend_from_slice(name.as_bytes());
        v
    }

    fn path_for(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Read a named document as raw decrypted bytes.
    ///
    /// Named `*_bytes` (rather than `load_doc`) so it can be an inherent
    /// method while `SecretProvider::load_doc` stays the generic JSON helper:
    /// inherent methods win name resolution, so the trait impl below cannot
    /// accidentally recurse into itself.
    pub fn load_doc_bytes(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
        Ok(self.read_doc(name)?.map(Zeroizing::new))
    }

    /// Write raw bytes under a document name.
    pub fn save_doc_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<()> {
        self.write_doc(name, bytes)
    }

    /// Read a named JSON document. `registry.bin` uses the registry subkey;
    /// anything else uses the secrets subkey.
    fn read_doc(&self, name: &str) -> AppResult<Option<Vec<u8>>> {
        let path = self.path_for(name);
        if !path.exists() {
            return Ok(None);
        }
        let env = std::fs::read(&path)?;
        if env.is_empty() {
            return Ok(None);
        }
        let plain = self.sealer_for(name).open(&env, &Self::aad(name))?;
        Ok(Some(plain))
    }

    fn sealer_for(&self, name: &str) -> &Sealer {
        if name == "registry.bin" {
            &self.registry_sealer
        } else {
            &self.secret_sealer
        }
    }

    /// Write a named document atomically: temp file, fsync, rename. A crash
    /// mid-write can therefore never truncate the real store.
    fn write_doc(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
        std::fs::create_dir_all(&self.dir)?;
        let env = self.sealer_for(name).seal(plaintext, &Self::aad(name))?;
        let target = self.path_for(name);
        atomic_write(&target, &env)
    }

    /// Store an opaque secret (e.g. the AniList token JSON).
    pub fn put_secret(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
        let file = secret_file_name(name);
        self.write_doc(&file, plaintext)
    }

    pub fn get_secret(&self, name: &str) -> AppResult<Option<Zeroizing<Vec<u8>>>> {
        let file = secret_file_name(name);
        Ok(self.read_doc(&file)?.map(Zeroizing::new))
    }

    pub fn delete_secret(&self, name: &str) -> AppResult<()> {
        let file = secret_file_name(name);
        let path = self.path_for(&file);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::from(e)),
        }
    }

    /// Wipe every encrypted document (used by "clear site data").
    pub fn wipe_secrets(&self) -> AppResult<usize> {
        let mut n = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("secret.") {
                std::fs::remove_file(entry.path())?;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// Secret names are attacker-influenced only insofar as the app defines them,
/// but we still refuse anything that could escape the store directory.
pub fn secret_file_name(name: &str) -> String {
    // Keep only a safe charset, then drop any run of dots: `../../etc/passwd`
    // becomes `etcpasswd`, so no input can produce a traversal or an empty
    // file name.
    let filtered: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(64)
        .collect();
    let clean = if filtered.is_empty() {
        "default".to_string()
    } else {
        filtered
    };
    debug_assert!(!clean.contains('.'));
    format!("secret.{clean}.bin")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::SecretProvider;
    use crate::sites::registry::{Category, Registry, SiteDraft};

    /// Build a store using the *real* platform key loader, pointed at a temp
    /// dir, so the tests exercise production code rather than a stand-in.
    fn store_with_dir(dir: &Path) -> SecureStore {
        let key = crate::secure::master_key::load_master_key(dir).expect("key");
        SecureStore::new(&key, dir).expect("store")
    }

    #[test]
    fn missing_registry_returns_defaults_with_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        let (reg, warn) = s.load_registry();
        assert_eq!(reg.sites.len(), 2);
        assert!(warn.is_none());
    }

    #[test]
    fn save_then_load_roundtrips_user_sites() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        let mut reg = Registry::new_default();
        reg.add(SiteDraft {
            name: "Kitsu".into(),
            url: "https://kitsu.app/".into(),
            category: Category::Tracking,
            letter: None,
            color: None,
            image: None,
        })
        .unwrap();
        s.save_registry(&reg).unwrap();

        // A *different* store instance (fresh key load) must read it back.
        let s2 = store_with_dir(dir.path());
        let (loaded, warn) = s2.load_registry();
        assert!(warn.is_none());
        assert_eq!(loaded.sites.len(), 3);
        assert!(loaded.sites.iter().any(|x| x.name == "Kitsu"));
    }

    #[test]
    fn registry_file_is_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        let mut reg = Registry::new_default();
        reg.add(SiteDraft {
            name: "SecretSiteName".into(),
            url: "https://hidden-name.example/".into(),
            category: Category::Watching,
            letter: None,
            color: None,
            image: None,
        })
        .unwrap();
        s.save_registry(&reg).unwrap();

        let raw = std::fs::read(dir.path().join("registry.bin")).unwrap();
        let as_text = String::from_utf8_lossy(&raw);
        assert!(!as_text.contains("SecretSiteName"), "name leaked to disk");
        assert!(
            !as_text.contains("hidden-name.example"),
            "url leaked to disk"
        );
        assert_eq!(&raw[0..4], b"AHB1", "must carry the envelope magic");
    }

    #[test]
    fn corrupt_registry_falls_back_to_defaults_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        s.save_registry(&Registry::new_default()).unwrap();

        // Flip bytes inside the ciphertext.
        let path = dir.path().join("registry.bin");
        let mut raw = std::fs::read(&path).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 0xff;
        std::fs::write(&path, raw).unwrap();

        let s2 = store_with_dir(dir.path());
        let (reg, warn) = s2.load_registry();
        assert_eq!(reg.sites.len(), 2, "must recover to defaults");
        assert!(warn.is_some(), "user must be told the store was unreadable");
    }

    #[test]
    fn secrets_roundtrip_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        assert!(s.get_secret("anilist").unwrap().is_none());

        s.put_secret("anilist", b"{\"access_token\":\"t0ken\"}")
            .unwrap();
        let got = s.get_secret("anilist").unwrap().unwrap();
        assert_eq!(&*got, b"{\"access_token\":\"t0ken\"}");

        // Not plaintext on disk.
        let raw = std::fs::read(dir.path().join("secret.anilist.bin")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("t0ken"));

        s.delete_secret("anilist").unwrap();
        assert!(s.get_secret("anilist").unwrap().is_none());
        // Deleting twice is not an error.
        s.delete_secret("anilist").unwrap();
    }

    #[test]
    fn secret_aad_prevents_swapping_between_names() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        s.put_secret("alpha", b"alpha-secret").unwrap();

        // Rename the file on disk: the AAD no longer matches.
        std::fs::rename(
            dir.path().join("secret.alpha.bin"),
            dir.path().join("secret.beta.bin"),
        )
        .unwrap();
        assert!(
            s.get_secret("beta").unwrap_err().code() == "crypto",
            "AAD must bind a secret to its name"
        );
    }

    #[test]
    fn wipe_secrets_leaves_registry_intact() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        s.save_registry(&Registry::new_default()).unwrap();
        s.put_secret("anilist", b"x").unwrap();
        s.put_secret("other", b"y").unwrap();
        assert_eq!(s.wipe_secrets().unwrap(), 2);
        assert!(s.get_secret("anilist").unwrap().is_none());
        assert!(dir.path().join("registry.bin").exists());
    }

    #[test]
    fn secret_file_name_rejects_traversal() {
        assert_eq!(secret_file_name("anilist"), "secret.anilist.bin");
        // Path separators and traversal are stripped, not interpreted.
        assert_eq!(secret_file_name("../../etc/passwd"), "secret.etcpasswd.bin");
    }

    #[test]
    fn unusable_secret_names_fall_back_rather_than_panic() {
        // A name made only of separators must still produce a real file name;
        // panicking here would take the whole app down over a bad key.
        assert_eq!(secret_file_name("///"), "secret.default.bin");
        assert_eq!(secret_file_name(""), "secret.default.bin");
        assert_eq!(secret_file_name(".."), "secret.default.bin");
        assert!(!secret_file_name("../../x").contains(".."));
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        assert!(!dir.path().join("x.tmp").exists());
    }
}
