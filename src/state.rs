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
    /// Waiting for the user's PIN to authorize a stake (SOL -> JitoSOL).
    VerifyingPinForStake { amount_sol: f64 },
    /// Waiting for the user's PIN to authorize a full unstake
    /// (JitoSOL -> SOL, with the yield fee skimmed off any gain).
    VerifyingPinForUnstake,
    /// User tapped "✏️ Custom SOL" on the yield/stake keyboard.
    EnteringCustomStakeAmount,
    /// User tapped "Reset Account" from Settings. Waiting for them to type
    /// the literal word RESET to confirm -- deliberately a typed
    /// confirmation, not just a button tap, since a stray tap (someone
    /// briefly grabbing an unlocked phone, a mis-tap) should not be enough
    /// to trigger something this destructive. Any other text cancels.
    ConfirmingReset,
    /// User tapped "➕ Add Wallet" -> "Generate New" from the wallet
    /// switcher. A fresh keypair has already been generated in memory
    /// (plaintext, held here only, along with its already-public address)
    /// -- waiting for the user's PIN so we can encrypt the private key
    /// under the SAME shared PIN as every other wallet slot and push it
    /// on as a new labeled sub-account.
    VerifyingPinForAddWallet { pending_pubkey: String, pending_wallet_secret_plain_b58: String },
}

impl Default for Awaiting {
    fn default() -> Self {
        Awaiting::None
    }
}

impl Awaiting {
    /// True for every state where the user's next typed message is a PIN
    /// or a raw private key -- i.e. something that must never be left
    /// sitting in Telegram chat history. If someone's phone is unlocked
    /// (by the owner, or by someone who's compromised the device), chat
    /// history is fully readable regardless of the bot's own encryption --
    /// a PIN typed a week ago is still sitting right there in the
    /// conversation. `handle_message` uses this to immediately delete the
    /// incoming message right after reading it, success or failure, so
    /// there's nothing left to find by scrolling back.
    pub fn expects_sensitive_input(&self) -> bool {
        matches!(
            self,
            Awaiting::SettingPin { .. }
                | Awaiting::VerifyingPinForExport
                | Awaiting::VerifyingPinForWithdraw { .. }
                | Awaiting::VerifyingPinForSubscribe
                | Awaiting::EnteringNewPin
                | Awaiting::VerifyingPinForChangePin { .. }
                | Awaiting::EnteringImportKey
                | Awaiting::VerifyingPinForImport { .. }
                | Awaiting::VerifyingPinForBuy { .. }
                | Awaiting::VerifyingPinForSell { .. }
                | Awaiting::VerifyingPinForStake { .. }
                | Awaiting::VerifyingPinForUnstake
                | Awaiting::VerifyingPinForAddWallet { .. }
        )
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

/// Max sub-accounts a single Telegram user can hold. Not a hard technical
/// ceiling -- picked as a sane stop against someone spamming "add wallet"
/// into a huge unbounded Vec on every save/load. Raise if you ever get a
/// legitimate request for more.
pub const MAX_WALLETS: usize = 30;

/// One sub-account ("W1", "W2", ...). Every wallet-holding field that used
/// to live directly on `UserRecord` now lives here instead, since a user
/// can have many of these under one shared PIN. `secret` is independently
/// encrypted per-slot (same envelope scheme as before, just one envelope
/// per wallet) -- there is no wrapping master key, so leaking one slot's
/// ciphertext plus a correct PIN only ever unlocks that one slot's
/// plaintext, same security property as the single-wallet version had.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSlot {
    /// Display label -- "W1", "W2", etc by default. No rename UI yet;
    /// straightforward to add later if wanted.
    pub label: String,
    pub pubkey: String,
    /// Envelope-encrypted private key. See crypto.rs -- this is useless
    /// without both the server pepper AND the user's PIN.
    pub secret: EnvelopeSecret,
    pub positions: Vec<Position>,
    /// Total SOL (lamports) currently committed to the yield feature in
    /// THIS wallet -- yield positions are tracked per-slot, same as
    /// trading positions, since each wallet is its own on-chain account.
    #[serde(default)]
    pub yield_principal_lamports: u64,
}

impl WalletSlot {
    pub fn new(label: String, pubkey: String, secret: EnvelopeSecret) -> Self {
        Self {
            label,
            pubkey,
            secret,
            positions: vec![],
            yield_principal_lamports: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub telegram_id: i64,
    /// Up to MAX_WALLETS sub-accounts, all protected by the same PIN.
    /// Always has at least one entry once the account exists (bootstrap
    /// and migration both guarantee this) -- `active()`/`active_mut()`
    /// are the only sanctioned way to reach "the wallet currently in use"
    /// and clamp defensively rather than ever indexing out of bounds.
    pub wallets: Vec<WalletSlot>,
    /// Index into `wallets` of the currently selected sub-account.
    #[serde(default)]
    pub active_wallet: usize,
    pub pin_lockout: PinLockout,
    pub awaiting: Awaiting,
    pub slippage_bps: u32,
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
    /// Auto-yield opt-in: when on, idle SOL in the user's ACTIVE wallet
    /// gets swept into JitoSOL automatically (via execute_stake), and
    /// gets auto-unstaked the moment it's needed for a buy (via
    /// execute_buy). "Automatic" here means "the bot does this without a
    /// manual stake/unstake tap whenever it already has a live decrypted
    /// key in hand" -- it does NOT mean the bot can act while the user is
    /// fully offline, since Wraith has no master key and never caches a
    /// PIN beyond the normal 15-minute trading-session window. See
    /// App::run_yield_sweep in handlers.rs for exactly when this fires.
    #[serde(default)]
    pub yield_auto_enabled: bool,
}

impl UserRecord {
    pub fn new(telegram_id: i64, pubkey: String, secret: EnvelopeSecret, default_slippage_bps: u32) -> Self {
        Self {
            telegram_id,
            wallets: vec![WalletSlot::new("W1".to_string(), pubkey, secret)],
            active_wallet: 0,
            pin_lockout: PinLockout::default(),
            awaiting: Awaiting::None,
            slippage_bps: default_slippage_bps,
            ref_code: format!("WRAITH_{}", telegram_id % 1_000_000),
            refs: 0,
            created_at: chrono_now(),
            gem_alerts: true,
            known_withdraw_addresses: vec![],
            subscription_expires_at: 0,
            yield_auto_enabled: false,
        }
    }

    /// The currently selected wallet. Clamps `active_wallet` defensively
    /// (rather than panicking) in case of any inconsistency -- `wallets`
    /// is guaranteed non-empty by construction (`new`) and by migration
    /// (`Db::recover_user_record`), so this never actually hits the
    /// zero-wallets case in practice, but a bad index is cheap to guard.
    pub fn active(&self) -> &WalletSlot {
        let i = self.active_wallet.min(self.wallets.len().saturating_sub(1));
        &self.wallets[i]
    }

    pub fn active_mut(&mut self) -> &mut WalletSlot {
        let i = self.active_wallet.min(self.wallets.len().saturating_sub(1));
        &mut self.wallets[i]
    }

    /// Next default label for a newly added wallet -- "W2", "W3", etc.
    /// Just counts existing slots rather than trying to fill gaps, so
    /// labels stay predictable even after slots are added over time.
    pub fn next_wallet_label(&self) -> String {
        format!("W{}", self.wallets.len() + 1)
    }
}

pub fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
