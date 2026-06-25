use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use rand::RngCore;

#[derive(Clone)]
pub struct Crypto {
    cipher: Aes256Gcm,
}

impl Crypto {
    pub fn new(master_key_b64: &str) -> Result<Self> {
        let key_bytes = general_purpose::STANDARD
            .decode(master_key_b64)
            .map_err(|e| anyhow!("WRAITH_MASTER_KEY is not valid base64: {e}"))?;
        if key_bytes.len() != 32 {
            return Err(anyhow!(
                "WRAITH_MASTER_KEY must decode to exactly 32 bytes, got {}",
                key_bytes.len()
            ));
        }
        let cipher = Aes256Gcm::new_from_slice(&key_bytes)
            .map_err(|e| anyhow!("failed to init cipher: {e}"))?;
        Ok(Self { cipher })
    }

    /// Encrypts plaintext, returns (nonce_b64, ciphertext_b64)
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(String, String)> {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| anyhow!("encryption failed: {e}"))?;

        Ok((
            general_purpose::STANDARD.encode(nonce_bytes),
            general_purpose::STANDARD.encode(ciphertext),
        ))
    }

    pub fn decrypt(&self, nonce_b64: &str, ciphertext_b64: &str) -> Result<Vec<u8>> {
        let nonce_bytes = general_purpose::STANDARD.decode(nonce_b64)?;
        let ciphertext = general_purpose::STANDARD.decode(ciphertext_b64)?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        self.cipher
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|e| anyhow!("decryption failed (wrong master key?): {e}"))
    }
}

/// Sha256 hash for PINs - we never store the raw PIN.
pub fn hash_pin(pin: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pin.as_bytes());
    general_purpose::STANDARD.encode(hasher.finalize())
}
