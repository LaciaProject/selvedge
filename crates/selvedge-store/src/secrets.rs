//! Envelope encryption for script secrets: AES-256-GCM under a master key that
//! comes from outside the database (KMS/Vault via External Secrets, or a
//! Kubernetes Secret in the minimal profile).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result};

#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// `key_hex` is 64 hex characters (32 bytes).
    pub fn from_hex(key_hex: &str) -> Result<Self> {
        let key = hex::decode(key_hex.trim()).context("master key must be hex")?;
        anyhow::ensure!(key.len() == 32, "master key must be 32 bytes (64 hex characters)");
        Ok(Self { cipher: Aes256Gcm::new_from_slice(&key)? })
    }

    /// `aad` binds the ciphertext to where it is stored, so a row copied to
    /// another script does not decrypt.
    pub fn seal(&self, plaintext: &[u8], aad: &str) -> Result<Vec<u8>> {
        let nonce: [u8; 12] = rand::random();
        let mut out = nonce.to_vec();
        out.extend(
            self.cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    aes_gcm::aead::Payload { msg: plaintext, aad: aad.as_bytes() },
                )
                .map_err(|_| anyhow::anyhow!("encryption failed"))?,
        );
        Ok(out)
    }

    pub fn open(&self, sealed: &[u8], aad: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(sealed.len() > 12, "ciphertext too short");
        let (nonce, ct) = sealed.split_at(12);
        self.cipher
            .decrypt(Nonce::from_slice(nonce), aes_gcm::aead::Payload { msg: ct, aad: aad.as_bytes() })
            .map_err(|_| anyhow::anyhow!("decryption failed: wrong master key or tampered row"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open() {
        let b = SecretBox::from_hex(&"11".repeat(32)).unwrap();
        let sealed = b.seal(b"s3cret", "a/s/N").unwrap();
        assert_eq!(b.open(&sealed, "a/s/N").unwrap(), b"s3cret");
        assert!(b.open(&sealed, "a/s/M").is_err());
        assert!(SecretBox::from_hex("11").is_err());
    }
}
