//! Encrypted-at-rest document store.
//!
//! Two things live here: the site [`Registry`] and opaque secret blobs (the
//! AniList access token). Both are AES-GCM envelopes; the AAD binds each
//! document to its filename so a blob cannot be swapped between stores.

use crate::error::{AppError, AppResult};
use crate::secure::crypto::{derive_subkey, Sealer};
use crate::secure::master_key::{Backend, MasterKey};
use std::path::{Path, PathBuf};

/// Shared serialization for every mutation of the encrypted store directory
/// (writes via `atomic_write`, single deletes, bulk wipes). A write that
/// started before a delete must not rename its tmp file after the delete has
/// removed the target, otherwise the secret would be resurrected. The lock
/// is held for the entire write (tmp create → sync → rename → dir fsync) and
/// for the entire delete, so the two cannot interleave. Both `SecureStore`
/// (desktop) and `AndroidStore` (via `crate::secure::store::STORE_LOCK`)
/// use this same lock, so the guarantee is process-wide.
pub(crate) static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
    /// The `STORE_LOCK` is held for the entire seal+write so a concurrent
    /// `delete_site_session` cannot delete the file after we have sealed but
    /// before we have written, which would otherwise resurrect the jar.
    fn write_doc(&self, name: &str, plaintext: &[u8]) -> AppResult<()> {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        std::fs::create_dir_all(&self.dir)?;
        let env = self.sealer_for(name).seal(plaintext, &Self::aad(name))?;
        let target = self.path_for(name);
        atomic_write_locked(&target, &env)
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
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        Self::delete_secret_locked(self, name)
    }

    fn delete_secret_locked(&self, name: &str) -> AppResult<()> {
        let file = secret_file_name(name);
        let path = self.path_for(&file);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::from(e)),
        }
    }

    /// Delete the three per-site session secrets for `site_id` atomically
    /// with respect to concurrent `put_secret` calls. Holding one `STORE_LOCK`
    /// for all three deletes prevents a concurrent `put_secret("cookies-…")`
    /// from resurrecting the jar between the cookie and localStorage deletes.
    pub(crate) fn delete_site_session(&self, site_id: &str) -> AppResult<()> {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
        // `profilemig` (the one-time Android profile-migration flag) rides
        // along so a cleared/removed site can never re-import a jar that is
        // about to be deleted. `profilegen` deliberately does NOT: the
        // generation counter must outlive the session so a stale registry
        // backup can never re-open an already-deleted profile name.
        for kind in ["cookies", "localstorage", "indexeddb", "profilemig"] {
            let name = format!("{kind}-{site_id}");
            Self::delete_secret_locked(self, &name)?;
        }
        Ok(())
    }

    /// Wipe every encrypted document (used by "clear site data").
    pub fn wipe_secrets(&self) -> AppResult<usize> {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
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

#[allow(dead_code)]
fn atomic_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| AppError::Storage("kilit alınamadı".into()))?;
    atomic_write_locked(path, bytes)
}

/// Create the temporary file of an atomic write: exclusive and owner-only.
///
/// `File::create` leaves the process umask in charge, so on Unix the temp file
/// was typically `0644` — readable by every local user, even though the payload
/// is encrypted. `create_new` also means an existing path is never truncated:
/// the unique name makes a collision practically impossible, and if it does
/// happen the safe outcome is an error, not a clobbered file. Windows has no
/// mode bits to set here and keeps its ACL default from `OpenOptions`.
fn create_private_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn atomic_write_locked(path: &Path, bytes: &[u8]) -> AppResult<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Unique temp name so two concurrent writes to the same target never share
    // the same `.tmp` file: `<name>.tmp.<pid>-<rand>`.
    let unique = {
        use rand::RngCore as _;
        let mut b = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut b);
        format!("{}-{}", std::process::id(), hex::encode(b))
    };
    // `with_extension` replaces only the final extension, so
    // `secret.foo.bin` -> `secret.foo.tmp.<unique>`; uniqueness is what
    // matters, not the exact stem.
    let tmp = path.with_extension(format!("tmp.{unique}"));
    let written = (|| -> std::io::Result<()> {
        let mut f = create_private_exclusive(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        // Never leave a half-written temp file next to the store: it is the
        // only place where plaintext-shaped intermediate state could sit.
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // The rename is the last step that can fail, and it leaves the temp
        // file behind. Remove it: the next read must not find a stray copy of
        // the store next to it, and a retry has to start from a clean
        // directory. A cleanup failure is ignored on purpose — the error the
        // caller sees has to be the one that describes why the write failed.
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    // Ensure the rename is durable: fsync the parent directory where the
    // filesystem supports it. Best-effort — a failure to open/fsync the dir
    // must not hide a successful write.
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
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

    #[cfg(unix)]
    #[test]
    fn the_store_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        atomic_write_locked(&path, b"encrypted-bytes").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the store (and its temp file) must not be readable by other local users"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"encrypted-bytes");
    }

    #[test]
    fn the_temp_file_is_created_exclusively_and_never_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("already-there");
        std::fs::write(&existing, b"keep me").unwrap();
        let opened = create_private_exclusive(&existing);
        let err = opened.expect_err("an existing path must not be opened");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
    }

    /// The temp file really exists and has been written when `rename` fails.
    ///
    /// The target is a non-empty directory, which `rename` refuses; nothing
    /// before it can fail that way (the parent exists, the temp name is unique
    /// and the directory is writable), so the error kind is proof that the
    /// write reached the rename step. `a_failed_write_leaves_the_existing_target_untouched`
    /// only covers `create_dir_all`, i.e. before a temp file exists at all.
    #[test]
    fn a_failed_rename_removes_its_temp_file_and_keeps_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("store.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), b"keep").unwrap();

        let err = atomic_write_locked(&target, b"new").expect_err("the target is a directory");
        // `rename` is the only step here that refuses a directory target — the
        // parent exists and is writable and the temp name is unique — so the
        // error kind is what proves the temp file had already been created and
        // written. Unix reports EISDIR (Linux) or ENOTDIR (some kernels);
        // Windows reports access-denied.
        #[cfg(unix)]
        {
            let kind = err.kind();
            let refused = kind == std::io::ErrorKind::IsADirectory;
            let refused = refused || kind == std::io::ErrorKind::NotADirectory;
            assert!(refused, "unexpected error kind: {kind:?}");
        }
        #[cfg(not(unix))]
        {
            let kind = err.kind();
            assert_eq!(kind, std::io::ErrorKind::PermissionDenied);
        }

        // The target is untouched …
        assert_eq!(std::fs::read(target.join("inside")).unwrap(), b"keep");
        // … and the temp file that was created and written is gone: the
        // directory holds the target and nothing else.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["store.json".to_string()]);
    }

    #[test]
    fn a_failed_write_leaves_the_existing_target_untouched() {
        // The parent is a regular file, so `create_dir_all` fails before the
        // temp file exists. Nothing about the existing target may change.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"original").unwrap();
        let target = blocker.join("store.json");
        assert!(atomic_write_locked(&target, b"new").is_err());
        assert_eq!(std::fs::read(&blocker).unwrap(), b"original");
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
        // Unique tmp scheme must also leave no stray .tmp.* file.
        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(stray.is_empty(), "unique tmp leaked: {stray:?}");
    }

    #[test]
    fn atomic_write_concurrent_same_path_is_not_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("concurrent.bin");
        let payloads: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("payload-{i}").into_bytes())
            .collect();
        let handles: Vec<_> = payloads
            .clone()
            .into_iter()
            .map(|payload| {
                let p = p.clone();
                std::thread::spawn(move || atomic_write(&p, &payload).unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let final_bytes = std::fs::read(&p).unwrap();
        assert!(
            payloads.contains(&final_bytes),
            "corrupt concurrent write produced {final_bytes:?}"
        );
        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            stray.is_empty(),
            "leaked tmp after concurrent writes: {stray:?}"
        );
    }

    #[test]
    fn atomic_write_overwrites_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("overwrite.bin");
        atomic_write(&p, b"first").unwrap();
        atomic_write(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn clear_site_data_must_not_touch_anilist_token() {
        // Exercises the real `delete_site_session` helper that
        // `remove_site` and `clear_site_data` call — not three manual
        // `delete_secret` calls — so a future refactor that widens the
        // prefix is caught. The AniList token lives under
        // `secret.anilist-token.bin` and must survive any site-data wipe.
        let dir = tempfile::tempdir().unwrap();
        let s = store_with_dir(dir.path());
        s.put_secret("anilist-token", b"secret-token").unwrap();
        s.put_secret("cookies-site-1", b"jar-1").unwrap();
        s.put_secret("localstorage-site-1", b"ls-1").unwrap();
        s.put_secret("indexeddb-site-1", b"idx-1").unwrap();
        // Production path: one atomic `delete_site_session` call, which holds
        // a single `STORE_LOCK` for all three deletes.
        s.delete_site_session("site-1").unwrap();
        assert!(
            s.get_secret("anilist-token").unwrap().is_some(),
            "anilist-token must survive site-data wipe"
        );
        assert!(s.get_secret("cookies-site-1").unwrap().is_none());
        assert!(s.get_secret("localstorage-site-1").unwrap().is_none());
        assert!(s.get_secret("indexeddb-site-1").unwrap().is_none());
    }

    #[test]
    fn concurrent_site_session_write_and_delete_does_not_resurrect() {
        // Deterministic linearization: a `put_secret` that has already
        // acquired `STORE_LOCK` (at the start of `write_doc`, before seal)
        // must block a concurrent `delete_site_session` until it finishes,
        // and if the write was in-flight *before* the delete, the delete
        // must win (the file must not reappear).
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        // Pre-seed a jar so there is something to delete, and capture a
        // valid sealed payload for "new-jar" without needing private APIs.
        let new_jar_bytes = {
            let s = store_with_dir(&dir_path);
            s.put_secret("cookies-site-racer", b"original").unwrap();
            s.put_secret("cookies-site-racer", b"new-jar").unwrap();
            std::fs::read(dir_path.join(secret_file_name("cookies-site-racer"))).unwrap()
        };
        // Restore original for the race.
        {
            let s = store_with_dir(&dir_path);
            s.put_secret("cookies-site-racer", b"original").unwrap();
        }
        // Simulate an in-flight `put` that has already acquired `STORE_LOCK`.
        let deleter_path = dir_path.clone();
        let deleter_handle = {
            let _guard = STORE_LOCK.lock().expect("store lock");
            let handle = std::thread::spawn(move || {
                let s = store_with_dir(&deleter_path);
                s.delete_site_session("site-racer").unwrap();
            });
            std::thread::sleep(std::time::Duration::from_millis(80));
            // While still holding `STORE_LOCK`, write the pre-sealed
            // `new-jar` payload via the locked helper (simulating the
            // in-flight `put`'s final rename). This happens before the
            // deleter can acquire the lock.
            {
                let path = dir_path.join(secret_file_name("cookies-site-racer"));
                atomic_write_locked(&path, &new_jar_bytes).unwrap();
            }
            handle
        };
        deleter_handle.join().unwrap();
        // After the deleter has run, the file must be gone.
        let s = store_with_dir(&dir_path);
        assert!(
            s.get_secret("cookies-site-racer").unwrap().is_none(),
            "deleted site jar must not resurrect after concurrent in-flight write"
        );
        // AniList token must never be touched.
        let s2 = store_with_dir(&dir_path);
        s2.put_secret("anilist-token", b"tok").unwrap();
        let after_token_bytes = {
            let s = store_with_dir(&dir_path);
            s.put_secret("cookies-site-racer", b"after-token").unwrap();
            std::fs::read(dir_path.join(secret_file_name("cookies-site-racer"))).unwrap()
        };
        // Restore to have something to delete again.
        {
            let s = store_with_dir(&dir_path);
            s.put_secret("cookies-site-racer", b"original").unwrap();
        }
        {
            let _guard = STORE_LOCK.lock().expect("store lock");
            let handle = std::thread::spawn({
                let path = dir_path.clone();
                move || {
                    let s = store_with_dir(&path);
                    s.delete_site_session("site-racer").unwrap();
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(30));
            {
                let path = dir_path.join(secret_file_name("cookies-site-racer"));
                atomic_write_locked(&path, &after_token_bytes).unwrap();
            }
            drop(_guard);
            handle.join().unwrap();
        }
        assert!(
            store_with_dir(&dir_path)
                .get_secret("anilist-token")
                .unwrap()
                .is_some(),
            "anilist-token must survive concurrent site wipe"
        );
        assert!(
            store_with_dir(&dir_path)
                .get_secret("cookies-site-racer")
                .unwrap()
                .is_none(),
            "site-racer must be deleted even after concurrent put"
        );
    }

    #[test]
    fn profile_dir_is_isolated_per_site_and_survives_sibling_delete() {
        // Uses the same `profile_dir_path` / `ensure_profile_dir` helpers
        // that `open_in_main_window_inner` and `remove_site` use, so a
        // future rename of the layout is exercised, not bypassed.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let d1 = crate::web::session::ensure_profile_dir(base, "site-1").unwrap();
        let d2 = crate::web::session::ensure_profile_dir(base, "site-2").unwrap();
        assert_ne!(d1, d2, "per-site dirs must be distinct");
        std::fs::write(d1.join("Cookies"), b"jar-1").unwrap();
        std::fs::write(d2.join("Cookies"), b"jar-2").unwrap();
        // Simulate `remove_site("site-1")` dir delete via production path.
        let expected = crate::web::session::profile_dir_path(base, "site-1");
        assert_eq!(d1, expected);
        std::fs::remove_dir_all(&d1).unwrap();
        assert!(!d1.exists());
        assert!(d2.exists());
        assert_eq!(std::fs::read(d2.join("Cookies")).unwrap(), b"jar-2");
    }
}
