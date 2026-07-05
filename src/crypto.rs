use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Everything needed to store and later decrypt one user's secret.
/// None of these fields are secret on their own -- `salt` is meant to be
/// public, and `wrapped_dek`/`ciphertext` are useless without BOTH the
/// server pepper AND the user's correct PIN. This is what goes in the DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeSecret {
    pub salt_b64: String,
    pub dek_nonce_b64: String,
    pub wrapped_dek_b64: String,
    pub data_nonce_b64: String,
    pub ciphertext_b64: String,
}

/// Argon2id params. These are deliberately expensive (~150-300ms per
/// attempt on typical server hardware) -- that's the point: it makes
/// brute-forcing a 4-6 digit PIN against a stolen DB dump slow instead
/// of instantaneous. Tune `m_cost` down only if your host genuinely
/// can't spare the RAM; never reduce below OWASP's minimums.
fn argon2() -> Result<Argon2<'static>> {
    // m_cost=19MiB, t_cost=2, p_cost=1 -- OWASP's Argon2id baseline.
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| anyhow!("bad argon2 params: {e}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

#[derive(Clone)]
pub struct Crypto {
    /// Server-side pepper. Load this from a SEPARATE secret store than
    /// your database backups (ideally a secrets manager / KMS, not the
    /// same .env living next to the DB file). The whole point of a
    /// pepper is that stealing the database alone is not enough.
    pepper: [u8; 32],
}

impl Crypto {
    pub fn new(pepper_b64: &str) -> Result<Self> {
        let bytes = general_purpose::STANDARD
            .decode(pepper_b64)
            .map_err(|e| anyhow!("WRAITH_PEPPER is not valid base64: {e}"))?;
        let pepper: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow!("WRAITH_PEPPER must decode to exactly 32 bytes"))?;
        Ok(Self { pepper })
    }

    /// Derives a key-encryption-key from (PIN + server pepper + per-user
    /// salt). Neither the PIN alone nor the pepper alone is enough.
    fn derive_kek(&self, pin: &str, salt: &[u8; 16]) -> Result<[u8; 32]> {
        let mut input = Vec::with_capacity(pin.len() + 32);
        input.extend_from_slice(pin.as_bytes());
        input.extend_from_slice(&self.pepper);

        let mut kek = [0u8; 32];
        let result = argon2()?.hash_password_into(&input, salt, &mut kek);
        input.zeroize();
        result.map_err(|e| anyhow!("key derivation failed: {e}"))?;
        Ok(kek)
    }

    /// Envelope-encrypts `plaintext` (the base58 private key) under a
    /// fresh random data-encryption-key (DEK), then wraps that DEK with
    /// a key derived from the user's PIN + server pepper. This is what
    /// you call once, when the user sets their PIN / creates their wallet.
    pub fn encrypt_with_pin(&self, pin: &str, plaintext: &[u8]) -> Result<EnvelopeSecret> {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);

        let mut kek = self.derive_kek(pin, &salt)?;
        let kek_cipher = Aes256Gcm::new_from_slice(&kek)
            .map_err(|e| anyhow!("cipher init failed: {e}"))?;

        let mut dek = [0u8; 32];
        OsRng.fill_bytes(&mut dek);
        let mut dek_nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut dek_nonce_bytes);
        let wrapped_dek = kek_cipher
            .encrypt(Nonce::from_slice(&dek_nonce_bytes), dek.as_ref())
            .map_err(|e| anyhow!("failed to wrap data key: {e}"))?;

        let data_cipher = Aes256Gcm::new_from_slice(&dek)
            .map_err(|e| anyhow!("cipher init failed: {e}"))?;
        let mut data_nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut data_nonce_bytes);
        let ciphertext = data_cipher
            .encrypt(Nonce::from_slice(&data_nonce_bytes), plaintext)
            .map_err(|e| anyhow!("encryption failed: {e}"))?;

        kek.zeroize();
        dek.zeroize();

        Ok(EnvelopeSecret {
            salt_b64: general_purpose::STANDARD.encode(salt),
            dek_nonce_b64: general_purpose::STANDARD.encode(dek_nonce_bytes),
            wrapped_dek_b64: general_purpose::STANDARD.encode(wrapped_dek),
            data_nonce_b64: general_purpose::STANDARD.encode(data_nonce_bytes),
            ciphertext_b64: general_purpose::STANDARD.encode(ciphertext),
        })
    }

    /// Attempts to decrypt with the given PIN. A wrong PIN fails the AEAD
    /// authentication tag check inside `decrypt` -- there is no separate
    /// PIN hash to leak or brute-force offline faster than this KDF allows.
    /// Caller MUST rate-limit calls to this (see lockout fields on
    /// UserRecord) since each call is an oracle for "was that the right PIN".
    pub fn decrypt_with_pin(&self, pin: &str, secret: &EnvelopeSecret) -> Result<Vec<u8>> {
        let salt: [u8; 16] = general_purpose::STANDARD
            .decode(&secret.salt_b64)?
            .try_into()
            .map_err(|_| anyhow!("corrupted salt"))?;
        let mut kek = self.derive_kek(pin, &salt)?;
        let kek_cipher = Aes256Gcm::new_from_slice(&kek)
            .map_err(|e| anyhow!("cipher init failed: {e}"))?;

        let dek_nonce = general_purpose::STANDARD.decode(&secret.dek_nonce_b64)?;
        let wrapped_dek = general_purpose::STANDARD.decode(&secret.wrapped_dek_b64)?;
        let mut dek = kek_cipher
            .decrypt(Nonce::from_slice(&dek_nonce), wrapped_dek.as_slice())
            .map_err(|_| anyhow!("wrong PIN"))?;
        kek.zeroize();

        let data_cipher = Aes256Gcm::new_from_slice(&dek)
            .map_err(|e| anyhow!("cipher init failed: {e}"))?;
        let data_nonce = general_purpose::STANDARD.decode(&secret.data_nonce_b64)?;
        let ciphertext = general_purpose::STANDARD.decode(&secret.ciphertext_b64)?;
        let plaintext = data_cipher
            .decrypt(Nonce::from_slice(&data_nonce), ciphertext.as_slice())
            .map_err(|_| anyhow!("decryption failed (corrupted data)"))?;

        dek.zeroize();
        Ok(plaintext)
    }

    /// Re-wraps the DEK under a new PIN without touching the underlying
    /// ciphertext. Use this for "change PIN" so you don't have to
    /// re-encrypt the private key itself.
    pub fn rewrap_with_new_pin(
        &self,
        old_pin: &str,
        new_pin: &str,
        secret: &EnvelopeSecret,
    ) -> Result<EnvelopeSecret> {
        let plaintext = self.decrypt_with_pin(old_pin, secret)?;
        let fresh = self.encrypt_with_pin(new_pin, &plaintext)?;
        // plaintext is a Vec<u8> owned locally -- zeroize before drop.
        let mut plaintext = plaintext;
        plaintext.zeroize();
        Ok(fresh)
    }
}
