use anyhow::{anyhow, Result};
use solana_sdk::signature::{Keypair, Signer};
use zeroize::Zeroize;

use crate::crypto::Crypto;
use crate::state::UserRecord;

/// A raw Solana wallet, before encryption. Never persist this directly -
/// it should only exist transiently in memory, and callers should
/// zeroize `private_key_base58` as soon as they're done with it.
pub struct Wallet {
    pub address: String,
    pub private_key_base58: String,
}

impl Drop for Wallet {
    fn drop(&mut self) {
        self.private_key_base58.zeroize();
    }
}

/// Generate a brand new, real Solana wallet.
pub fn create_wallet() -> Wallet {
    let keypair = Keypair::new();
    Wallet {
        address: keypair.pubkey().to_string(),
        private_key_base58: bs58::encode(keypair.to_bytes()).into_string(),
    }
}

/// Import an existing wallet from a Base58 private key (the format
/// Phantom/Solflare export).
pub fn import_wallet(private_key: &str) -> Result<Wallet> {
    let trimmed = private_key.trim();
    let decoded = bs58::decode(trimmed)
        .into_vec()
        .map_err(|_| anyhow!("Invalid Base58 private key"))?;
    let keypair = Keypair::from_bytes(decoded.as_slice())
        .map_err(|_| anyhow!("Invalid Solana keypair - check you copied the full key"))?;
    Ok(Wallet {
        address: keypair.pubkey().to_string(),
        private_key_base58: trimmed.to_string(),
    })
}

/// Generates a new wallet and immediately envelope-encrypts it under the
/// user's PIN. There is no path to create a wallet without a PIN.
pub fn generate_encrypted_wallet(crypto: &Crypto, pin: &str) -> Result<(String, crate::crypto::EnvelopeSecret)> {
    let wallet = create_wallet();
    let secret = crypto.encrypt_with_pin(pin, wallet.private_key_base58.as_bytes())?;
    Ok((wallet.address.clone(), secret))
}

/// Validates a user-supplied private key, then envelope-encrypts it under
/// the user's PIN for storage.
pub fn import_encrypted_wallet(
    crypto: &Crypto,
    pin: &str,
    private_key_b58: &str,
) -> Result<(String, crate::crypto::EnvelopeSecret)> {
    let wallet = import_wallet(private_key_b58)?;
    let secret = crypto.encrypt_with_pin(pin, wallet.private_key_base58.as_bytes())?;
    Ok((wallet.address.clone(), secret))
}

/// Decrypts and reconstructs the user's Keypair for signing. Call this
/// only at the moment a signature is actually needed, use it immediately,
/// and drop it -- never hold the decrypted key or Keypair longer than
/// necessary. Caller MUST have already checked `user.pin_lockout` before
/// calling this; this function has no rate limiting of its own.
///
/// Returns an error on wrong PIN -- callers must feed that into
/// `PinLockout::record_failure` and save the user record.
pub fn load_keypair(crypto: &Crypto, pin: &str, user: &UserRecord) -> Result<Keypair> {
    let mut plaintext = crypto.decrypt_with_pin(pin, &user.secret)?;
    let private_key_b58 =
        String::from_utf8(plaintext.clone()).map_err(|e| anyhow!("corrupted key data: {e}"))?;
    plaintext.zeroize();
    let decoded = bs58::decode(&private_key_b58).into_vec()?;
    let result = Keypair::from_bytes(decoded.as_slice()).map_err(|e| anyhow!("corrupted key data: {e}"));
    result
}

/// Returns the base58 private key string for export. Caller is
/// responsible for: PIN + lockout check happening BEFORE this is called,
/// warning the user heavily, and NEVER logging the returned string.
///
/// IMPORTANT CAVEAT: this string, once sent back to the user over
/// Telegram chat, is no longer under your control -- it has already
/// touched Telegram's infrastructure the instant it's sent, regardless
/// of any "self-deletes in 60s" behavior you add on top. See the note
/// in handlers.rs about safer export UX (Telegram WebApp popup instead
/// of a chat message) if you want to actually close this gap rather
/// than just narrow the window.
pub fn export_private_key_b58(crypto: &Crypto, pin: &str, user: &UserRecord) -> Result<String> {
    let plaintext = crypto.decrypt_with_pin(pin, &user.secret)?;
    String::from_utf8(plaintext).map_err(|e| anyhow!("corrupted key data: {e}"))
}
