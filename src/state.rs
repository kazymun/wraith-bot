use serde::{Deserialize, Serialize};

use crate::crypto::EnvelopeSecret;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Awaiting {
    None,
    /// PIN is now mandatory at signup -- there is no "no PIN set, skip
    /// the check" path anymore. A wallet cannot exist without a PIN
    /// protecting it.
    SettingPin { pending_wallet_secret_plain_b58: String },
    VerifyingPinForExport,
    VerifyingPinForWithdraw { dest: String, amount_sol: f64 },
    /// User already typed+validated their new PIN (held here); we're now
    /// waiting for their CURRENT PIN to authorize the change.
    VerifyingPinForChangePin { new_pin: String },
    /// Waiting for the user to type their desired new PIN, before we ask
    /// them to confirm it with their current one.
    EnteringNewPin,
    VerifyingPinForImport { pending_key_b58: String },
    /// No active trading session -- unlocking it (15 min) requires the PIN
    /// once, then this buy proceeds automatically.
    VerifyingPinForBuy { ca: String, amount_sol: f64 },
    /// Same as above, for a sell. `pct` (1-100) is how much of the current
    /// token balance to sell -- selected before the PIN prompt via the
    /// percentage keyboard (or a custom typed value).
    VerifyingPinForSell { ca: String, pct: u8 },
    EnteringBuyCA,
    /// Waiting for a CA *or* a coin name/symbol to sell -- resolved against
    /// the user's own open positions first, then DexScreener search, before
    /// showing the sell-percentage keyboard.
    EnteringSellCA,
    /// User tapped "✏️ Custom %" on the sell-percentage keyboard; waiting
    /// for them to type a number 1-100.
    EnteringCustomSellPercent { ca: String },
    EnteringWithdrawAddress,
    EnteringRugScanCA,
    EnteringImportKey,
    EnteringCustomBuyAmount { ca: String },
    /// Waiting for the user's PIN to authorize a subscription payment
    /// (plain SOL transfer to FEE_WALLET, built inline in
    /// App::do_subscribe_payment in handlers.rs).
    VerifyingPinForSubscribe,
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

/// Escalating lockout after repeated wrong PINs. Each failed attempt to
/// decrypt (export, withdraw, change-pin) increments `failed_attempts`
/// and sets `locked_until`. This is enforced in handlers.rs BEFORE ever
/// calling `crypto.decrypt_with_pin` -- the lockout has to happen outside
/// the crypto call, since the crypto call itself is the brute-force oracle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PinLockout {
    pub failed_attempts: u32,
    pub locked_until: i64, // unix ts; 0 = not locked
}

impl PinLockout {
    /// Returns seconds remaining locked, or 0 if not locked.
    pub fn seconds_remaining(&self, now: i64) -> i64 {
        (self.locked_until - now).max(0)
    }

    pub fn record_failure(&mut self, now: i64) {
        self.failed_attempts += 1;
        // Exponential backoff: 3 fails -> 30s, 4 -> 2min, 5 -> 15min,
        // 6 -> 1hr, 7+ -> 24hr. Tune to taste but never let this hit 0
        // again once failures start piling up.
        let lock_secs: i64 = match self.failed_attempts {
            0..=2 => 0,
            3 => 30,
            4 => 120,
            5 => 900,
            6 => 3_600,
            _ => 86_400,
        };
        if lock_secs > 0 {
            self.locked_until = now + lock_secs;
        }
    }

    pub fn record_success(&mut self) {
        self.failed_attempts = 0;
        self.locked_until = 0;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub telegram_id: i64,
    pub pubkey: String,
    /// Envelope-encrypted private key. See crypto.rs -- this is useless
    /// without both the server pepper AND the user's PIN.
    pub secret: EnvelopeSecret,
    pub pin_lockout: PinLockout,
    pub awaiting: Awaiting,
    pub slippage_bps: u32,
    pub positions: Vec<Position>,
    pub ref_code: String,
    pub refs: u32,
    pub created_at: i64,
    #[serde(default = "default_true")]
    pub gem_alerts: bool,
    /// Addresses the user has previously withdrawn to. First-time
    /// withdrawals to a NEW address get an extra confirmation + a short
    /// mandatory delay -- see handlers.rs withdraw flow. This blunts
    /// account-takeover drains and clipboard-hijack attacks.
    #[serde(default)]
    pub known_withdraw_addresses: Vec<String>,
    /// Unix timestamp (seconds) until which this user's subscription is
    /// active. 0 (the default for existing users) means "never
    /// subscribed" -- always treated as expired. Checked in handlers.rs
    /// before allowing any bot functionality.
    #[serde(default)]
    pub subscription_expires_at: i64,
}

impl UserRecord {
    pub fn new(telegram_id: i64, pubkey: String, secret: EnvelopeSecret, default_slippage_bps: u32) -> Self {
        Self {
            telegram_id,
            pubkey,
            secret,
            pin_lockout: PinLockout::default(),
            awaiting: Awaiting::None,
            slippage_bps: default_slippage_bps,
            positions: vec![],
            ref_code: format!("WRAITH_{}", telegram_id % 1_000_000),
            refs: 0,
            created_at: chrono_now(),
            gem_alerts: true,
            known_withdraw_addresses: vec![],
            subscription_expires_at: 0,
        }
    }
}

pub fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
