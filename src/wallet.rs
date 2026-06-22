use anyhow::{anyhow, Result};
use solana_sdk::signature::{Keypair, Signer};

use crate::crypto::Crypto;
use crate::state::UserRecord;

/// A raw Solana wallet, before encryption. Never persist this directly -
/// it should only exist transiently in memory.
pub struct Wallet {
    pub address: String,
    pub private_key_base58: String,
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

/// Generates a new wallet and immediately encrypts the private key for
/// storage. Returns (pubkey, nonce_b64, cipher_b64) - this is what gets
/// saved to the DB, never the raw key.
pub fn generate_encrypted_wallet(crypto: &Crypto) -> Result<(String, String, String)> {
    let wallet = create_wallet();
    let (nonce_b64, cipher_b64) = crypto.encrypt(wallet.private_key_base58.as_bytes())?;
    Ok((wallet.address, nonce_b64, cipher_b64))
}

/// Validates a user-supplied private key, then encrypts it for storage.
pub fn import_encrypted_wallet(crypto: &Crypto, private_key_b58: &str) -> Result<(String, String, String)> {
    let wallet = import_wallet(private_key_b58)?;
    let (nonce_b64, cipher_b64) = crypto.encrypt(wallet.private_key_base58.as_bytes())?;
    Ok((wallet.address, nonce_b64, cipher_b64))
}

/// Decrypts and reconstructs the user's Keypair for signing. Call this
/// only at the moment a signature is actually needed - never hold the
/// decrypted key in memory longer than necessary.
pub fn load_keypair(crypto: &Crypto, user: &UserRecord) -> Result<Keypair> {
    let plaintext = crypto.decrypt(&user.enc_nonce, &user.enc_cipher)?;
    let private_key_b58 = String::from_utf8(plaintext).map_err(|e| anyhow!("corrupted key data: {e}"))?;
    let decoded = bs58::decode(&private_key_b58).into_vec()?;
    Keypair::from_bytes(decoded.as_slice()).map_err(|e| anyhow!("corrupted key data: {e}"))
}

/// Returns the base58 private key string for export. Caller is
/// responsible for gating this behind PIN confirmation.
pub fn export_private_key_b58(crypto: &Crypto, user: &UserRecord) -> Result<String> {
    let plaintext = crypto.decrypt(&user.enc_nonce, &user.enc_cipher)?;
    String::from_utf8(plaintext).map_err(|e| anyhow!("corrupted key data: {e}"))
}
