//! Authenticated encryption for everything AnimeHub writes to disk.
//!
//! Format (`v1` envelope, base64-encoded when persisted):
//!
//! ```text
//! "AHB1" | version:u8 | nonce:12 bytes | ciphertext+GCM tag
//! ```
//!
//! AES-256-GCM gives both confidentiality and integrity, so a tampered file
//! fails loudly instead of silently yielding half-decoded JSON.

use crate::error::{AppError, AppResult};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use zeroize::Zeroizing;

pub const MAGIC: &[u8; 4] = b"AHB1";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const MIN_LEN: usize = MAGIC.len() + 1 + NONCE_LEN + 16; // + GCM tag

pub struct Sealer {
    cipher: Aes256Gcm,
}

impl Sealer {
    /// Build a sealer from a 32-byte key. The key is not retained after the
    /// cipher is constructed.
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Sealer {
            cipher: Aes256Gcm::new_from_slice(key).expect("32-byte key is always valid"),
        }
    }

    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let payload = Payload {
            msg: plaintext,
            aad,
        };
        let ct = self.cipher.encrypt(nonce, payload)?;

        let mut out = Vec::with_capacity(MAGIC.len() + 1 + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.push(1);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, envelope: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        if envelope.len() < MIN_LEN {
            return Err(AppError::Crypto("veri bozuk veya eksik".into()));
        }
        if &envelope[0..4] != MAGIC {
            return Err(AppError::Crypto("bilinmeyen veri biçimi".into()));
        }
        if envelope[4] != 1 {
            return Err(AppError::Crypto(format!(
                "desteklenmeyen sürüm: {}",
                envelope[4]
            )));
        }

        let nonce = Nonce::from_slice(&envelope[5..5 + NONCE_LEN]);
        let payload = Payload {
            msg: &envelope[5 + NONCE_LEN..],
            aad,
        };
        self.cipher
            .decrypt(nonce, payload)
            .map_err(|_| AppError::Crypto("şifre çözülemedi (anahtar yanlış olabilir)".into()))
    }

    /// Convenience wrappers for JSON documents.
    pub fn seal_json<T: serde::Serialize>(&self, value: &T, aad: &[u8]) -> AppResult<Vec<u8>> {
        let bytes = serde_json::to_vec(value)?;
        self.seal(&bytes, aad)
    }

    pub fn open_json<T: serde::de::DeserializeOwned>(
        &self,
        envelope: &[u8],
        aad: &[u8],
    ) -> AppResult<T> {
        let plain = Zeroizing::new(self.open(envelope, aad)?);
        serde_json::from_slice(&plain).map_err(AppError::from)
    }

    /// Base64 form for embedding an envelope inside another JSON document.
    pub fn seal_b64(&self, plaintext: &[u8], aad: &[u8]) -> AppResult<String> {
        use base64::Engine as _;
        Ok(base64::engine::general_purpose::STANDARD.encode(self.seal(plaintext, aad)?))
    }

    pub fn open_b64(&self, encoded: &str, aad: &[u8]) -> AppResult<Vec<u8>> {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| AppError::Crypto("base64 çözülemedi".into()))?;
        self.open(&raw, aad)
    }
}

/// Derive a 32-byte subkey from the master key for a specific purpose, so that
/// one leaked ciphertext cannot be replayed against another store.
pub fn derive_subkey(master: &[u8; KEY_LEN], purpose: &str) -> [u8; KEY_LEN] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"animehub/subkey/v1/");
    h.update(purpose.as_bytes());
    h.update(master);
    let out = h.finalize();
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&out);
    key
}

/// Generate a fresh random 32-byte key.
pub fn generate_key() -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    fn key() -> [u8; 32] {
        [7u8; 32]
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Doc {
        a: u32,
        b: String,
    }

    #[test]
    fn roundtrip_preserves_bytes() {
        let s = Sealer::new(&key());
        let msg = b"merhaba \xc3\xa7erezler";
        let env = s.seal(msg, b"aad").unwrap();
        assert_eq!(s.open(&env, b"aad").unwrap(), msg);
    }

    #[test]
    fn envelope_starts_with_magic_and_version() {
        let s = Sealer::new(&key());
        let env = s.seal(b"x", b"").unwrap();
        assert_eq!(&env[0..4], b"AHB1");
        assert_eq!(env[4], 1);
        assert!(env.len() >= MIN_LEN);
    }

    #[test]
    fn nonce_is_random_so_ciphertexts_differ() {
        let s = Sealer::new(&key());
        let a = s.seal(b"same", b"").unwrap();
        let b = s.seal(b"same", b"").unwrap();
        assert_ne!(a, b, "reusing a nonce would be a serious bug");
    }

    #[test]
    fn wrong_aad_fails() {
        let s = Sealer::new(&key());
        let env = s.seal(b"secret", b"registry").unwrap();
        assert!(s.open(&env, b"other").is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let env = Sealer::new(&key()).seal(b"secret", b"").unwrap();
        let other = Sealer::new(&[9u8; 32]);
        assert!(other.open(&env, b"").is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let s = Sealer::new(&key());
        let mut env = s.seal(b"secret-data", b"").unwrap();
        let last = env.len() - 1;
        env[last] ^= 0x01;
        assert!(s.open(&env, b"").is_err());
    }

    #[test]
    fn truncated_envelope_fails() {
        let s = Sealer::new(&key());
        assert!(s.open(b"AHB1", b"").is_err());
        assert!(s.open(&[], b"").is_err());
    }

    #[test]
    fn bad_magic_fails() {
        let s = Sealer::new(&key());
        let mut env = s.seal(b"x", b"").unwrap();
        env[0] = b'X';
        assert!(s.open(&env, b"").is_err());
    }

    #[test]
    fn unsupported_version_fails() {
        let s = Sealer::new(&key());
        let mut env = s.seal(b"x", b"").unwrap();
        env[4] = 9;
        let err = s.open(&env, b"").unwrap_err();
        assert_eq!(err.code(), "crypto");
    }

    #[test]
    fn json_roundtrip() {
        let s = Sealer::new(&key());
        let doc = Doc {
            a: 42,
            b: "çerez".into(),
        };
        let env = s.seal_json(&doc, b"doc").unwrap();
        let back: Doc = s.open_json(&env, b"doc").unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn base64_roundtrip() {
        let s = Sealer::new(&key());
        let b64 = s.seal_b64(b"payload", b"aad").unwrap();
        assert!(b64.chars().all(|c| c.is_ascii_graphic() || c == '='));
        assert_eq!(s.open_b64(&b64, b"aad").unwrap(), b"payload");
    }

    #[test]
    fn base64_garbage_is_rejected() {
        let s = Sealer::new(&key());
        assert!(s.open_b64("!!!not base64!!!", b"").is_err());
        assert!(s.open_b64("", b"").is_err());
    }

    #[test]
    fn subkeys_are_purpose_bound_and_stable() {
        let m = key();
        let a = derive_subkey(&m, "registry");
        let b = derive_subkey(&m, "tokens");
        assert_ne!(a, b);
        assert_eq!(a, derive_subkey(&m, "registry"));
        assert_ne!(a, derive_subkey(&[8u8; 32], "registry"));
    }

    #[test]
    fn generated_keys_are_unique() {
        assert_ne!(generate_key(), generate_key());
    }
}
