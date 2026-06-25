use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Awaiting {
    None,
    SettingPin,
    VerifyingPinForExport,
    VerifyingPinForWithdraw { dest: String, amount_sol: f64 },
    EnteringBuyCA,
    EnteringSellCA,
    EnteringWithdrawAddress,
    EnteringWithdrawPin, // legacy placeholder, unused directly
    EnteringRugScanCA,
    EnteringImportKey,
    VerifyingPinForChangePin,
    VerifyingPinForImport,
    EnteringCustomBuyAmount { ca: String },
}

impl Default for Awaiting {
    fn default() -> Self {
        Awaiting::None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub mint: String,
    pub symbol: String,
    pub sol_spent: f64,
    pub tokens_received_est: f64,
    pub timestamp: i64,
    #[serde(default)]
    pub entry_price_usd: f64,
    #[serde(default = "default_decimals")]
    pub decimals: u8,
}

fn default_decimals() -> u8 {
    9
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub telegram_id: i64,
    pub pubkey: String,
    pub enc_nonce: String,
    pub enc_cipher: String,
    pub pin_hash: Option<String>,
    pub awaiting: Awaiting,
    pub slippage_bps: u32,
    pub positions: Vec<Position>,
    pub ref_code: String,
    pub refs: u32,
    pub created_at: i64,
    #[serde(default = "default_true")]
    pub gem_alerts: bool,
}

impl UserRecord {
    pub fn new(telegram_id: i64, pubkey: String, enc_nonce: String, enc_cipher: String, default_slippage_bps: u32) -> Self {
        Self {
            telegram_id,
            pubkey,
            enc_nonce,
            enc_cipher,
            pin_hash: None,
            awaiting: Awaiting::None,
            slippage_bps: default_slippage_bps,
            positions: vec![],
            ref_code: format!("WRAITH_{}", telegram_id % 1_000_000),
            refs: 0,
            created_at: chrono_now(),
            gem_alerts: true,
        }
    }
}

pub fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
