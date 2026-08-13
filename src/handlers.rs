use crate::crypto::Crypto;
use crate::db::Db;
use crate::dexscreener;
use crate::jupiter::{out_amount, price_impact_pct, Jupiter, JITO_SOL_MINT, SOL_MINT};
use crate::keyboards as kb;
use crate::pumpportal::PumpEvent;
use crate::rpc::SolanaRpc;
use crate::state::{Awaiting, Position, UserRecord};
use crate::telegram::{TgClient, TgMessage};
use crate::wallet;
use anyhow::{anyhow, Result};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::system_instruction;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use zeroize::Zeroize;

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;
const TRADING_SESSION_SECS: i64 = 15 * 60;
/// Same rough migration threshold as pumpportal.rs, used only for the
/// human-readable "~X% toward migration" display in watch alerts.
const MIGRATION_SOL_APPROX_FOR_DISPLAY: f64 = 85.0;

/// One scannable candidate for the AI Gem Scanner, whichever source it
/// came from -- lets DexScreener-based gems and pump.fun (pre/post-
/// migration) candidates get merged, sorted, and rendered as a single
/// consistent list.
struct GemEntry {
    ca: String,
    name: String,
    symbol: String,
    score: i32,
    tier: &'static str,
    notes: Vec<String>,
    mc: Option<f64>,
    liq: Option<f64>,
    change1h: Option<f64>,
    is_fresh: bool,
    pre_migration: bool,
}

/// In-memory-only cache of a decrypted keypair, so a user who already
/// entered their PIN once doesn't get asked again on every single trade.
/// NEVER written to disk, wiped on process restart, and zeroized on drop.
/// Export and withdraw NEVER consult this cache -- they always require a
/// fresh PIN, no matter how recently the user unlocked trading.
struct TradingSession {
    keypair_bytes: [u8; 64],
    expires_at: i64,
}

impl Drop for TradingSession {
    fn drop(&mut self) {
        self.keypair_bytes.zeroize();
    }
}

#[derive(Clone)]
pub struct App {
    pub tg: TgClient,
    pub db: Db,
    pub crypto: Crypto,
    pub rpc: SolanaRpc,
    pub jup: Jupiter,
    pub default_slippage_bps: u32,
    pub fee_wallet: String,
    pub min_pin_length: usize,
    /// Telegram IDs that always have access, no subscription required.
    pub free_access_ids: Vec<i64>,
    /// Monthly subscription price in lamports. Set once via
    /// SUBSCRIPTION_SOL in .env (config.rs) -- every prompt/payment
    /// below reads it from here, so there's no second hardcoded price
    /// to fall out of sync with.
    pub subscription_lamports: u64,
    /// Max lamports Jupiter may spend per-swap bidding for faster block
    /// inclusion. See config.rs (MAX_PRIORITY_FEE_SOL) and jupiter.rs
    /// (get_swap_transaction) for how this is actually used.
    pub max_priority_fee_lamports: u64,
    /// Cut of staking GAINS ONLY (never principal) taken on unstake, in
    /// basis points. See config.rs (YIELD_FEE_BPS) and execute_unstake.
    pub yield_fee_bps: u32,
    /// Liquid SOL always left un-staked in an auto-yield user's active
    /// wallet. See config.rs (YIELD_RESERVE_SOL).
    pub yield_reserve_lamports: u64,
    sessions: Arc<Mutex<HashMap<i64, TradingSession>>>,
}

impl App {
    pub fn new(
        tg: TgClient,
        db: Db,
        crypto: Crypto,
        rpc: SolanaRpc,
        jup: Jupiter,
        default_slippage_bps: u32,
        fee_wallet: String,
        min_pin_length: usize,
        free_access_ids: Vec<i64>,
        subscription_lamports: u64,
        max_priority_fee_lamports: u64,
        yield_fee_bps: u32,
        yield_reserve_lamports: u64,
    ) -> Self {
        Self {
            tg,
            db,
            crypto,
            rpc,
            jup,
            default_slippage_bps,
            fee_wallet,
            min_pin_length,
            free_access_ids,
            subscription_lamports,
            max_priority_fee_lamports,
            yield_fee_bps,
            yield_reserve_lamports,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Human-readable subscription price, e.g. "0.3 SOL". Used everywhere
    /// the price is shown to a user, so the display always matches
    /// whatever SUBSCRIPTION_SOL is actually set to.
    fn subscription_sol_str(&self) -> String {
        let sol = self.subscription_lamports as f64 / LAMPORTS_PER_SOL;
        // Trim trailing zeros (e.g. "0.300" -> "0.3") without pulling in
        // an extra formatting crate.
        let s = format!("{sol:.9}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        format!("{s} SOL")
    }

    // ---------- trading session cache ----------

    /// Whether this user currently has access. Three ways in:
    /// 1. The paywall is disabled entirely (SUBSCRIPTION_SOL=0 in .env) --
    ///    everyone gets access, no payment required, no subscribe prompt
    ///    ever shown. This is the current setting.
    /// 2. Their Telegram ID is on the free-access allowlist (FREE_ACCESS_IDS).
    /// 3. They have an active paid subscription.
    fn has_access(&self, user: &UserRecord) -> bool {
        self.subscription_lamports == 0
            || self.free_access_ids.contains(&user.telegram_id)
            || user.subscription_expires_at > crate::state::chrono_now()
    }

    fn session_keypair(&self, telegram_id: i64) -> Option<Keypair> {
        let now = crate::state::chrono_now();
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(&telegram_id) {
            Some(s) if s.expires_at > now => Keypair::from_bytes(&s.keypair_bytes).ok(),
            Some(_) => {
                sessions.remove(&telegram_id);
                None
            }
            None => None,
        }
    }

    fn store_session(&self, telegram_id: i64, keypair: &Keypair) {
        let now = crate::state::chrono_now();
        self.sessions.lock().unwrap().insert(
            telegram_id,
            TradingSession {
                keypair_bytes: keypair.to_bytes(),
                expires_at: now + TRADING_SESSION_SECS,
            },
        );
    }

    fn clear_session(&self, telegram_id: i64) {
        self.sessions.lock().unwrap().remove(&telegram_id);
    }

    /// Enforces lockout BEFORE ever calling into crypto (the crypto call
    /// itself is a brute-force oracle -- see crypto.rs), then attempts to
    /// decrypt. Always persists the updated lockout counters, win or lose.
    fn try_pin(&self, user: &mut UserRecord, pin: &str) -> Result<Option<Keypair>> {
        let now = crate::state::chrono_now();
        if user.pin_lockout.seconds_remaining(now) > 0 {
            return Ok(None);
        }
        match wallet::load_keypair(&self.crypto, pin, &user.active().secret) {
            Ok(kp) => {
                user.pin_lockout.record_success();
                self.db.save_user(user)?;
                Ok(Some(kp))
            }
            Err(_) => {
                user.pin_lockout.record_failure(now);
                self.db.save_user(user)?;
                Ok(None)
            }
        }
    }

    // ---------- user bootstrap ----------

    /// PIN is mandatory before a wallet's secret can be encrypted, so a
    /// brand new user gets a REAL wallet + pubkey immediately (so we can
    /// show a deposit address), but the private key sits briefly as
    /// plaintext inside `Awaiting::SettingPin` -- not decryptable, not
    /// used for anything -- until their very first reply (their chosen
    /// PIN) replaces the placeholder `secret` with a real encrypted one.
    pub fn get_or_create_user(&self, telegram_id: i64) -> Result<UserRecord> {
        if let Some(u) = self.db.get_user(telegram_id)? {
            return Ok(u);
        }
        let wallet = wallet::create_wallet();
        let placeholder_secret = self
            .crypto
            .encrypt_with_pin("", wallet.private_key_base58.as_bytes())?;
        let mut user = UserRecord::new(
            telegram_id,
            wallet.address.clone(),
            placeholder_secret,
            self.default_slippage_bps,
        );
        user.awaiting = Awaiting::SettingPin {
            pending_wallet_secret_plain_b58: wallet.private_key_base58.clone(),
        };
        self.db.save_user(&user)?;
        Ok(user)
    }

    async fn main_menu_text(&self, chat_id: i64, user: &UserRecord) -> String {
        let balance_sol = self
            .rpc
            .get_balance_lamports(&user.active().pubkey)
            .await
            .map(|l| l as f64 / LAMPORTS_PER_SOL)
            .unwrap_or(-1.0);

        let balance_line = if balance_sol >= 0.0 {
            format!("{balance_sol:.4} SOL")
        } else {
            "(couldn't fetch - RPC issue)".to_string()
        };

        let _ = chat_id;
        format!(
            "👻 <b>Wraith</b> — Solana Memecoin Sniper\n\n\
            💰 <b>Balance:</b> {balance_line}\n\
            👛 <b>Wallet:</b> <code>{}</code>\n\n\
            Select an option:",
            short_wallet(&user.active().pubkey)
        )
    }

    pub async fn show_main(&self, chat_id: i64, telegram_id: i64) -> Result<()> {
        let user = self.get_or_create_user(telegram_id)?;
        if !self.has_access(&user) {
            self.send_subscribe_prompt(chat_id, &user).await?;
            return Ok(());
        }
        let text = self.main_menu_text(chat_id, &user).await;
        self.tg.send_html(chat_id, &text, Some(kb::main_menu())).await?;
        Ok(())
    }

    /// Shown in place of the main menu (or any gated action) whenever the
    /// user's subscription has lapsed or never started. Deliberately still
    /// shows their wallet address/balance so they can deposit funds to pay.
    async fn send_subscribe_prompt(&self, chat_id: i64, user: &UserRecord) -> Result<()> {
        let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
        let price = self.subscription_sol_str();
        self.tg.send_html(
            chat_id,
            &format!(
                "🔒 <b>Wraith is subscription-only</b>\n\n💳 <b>{price} / month</b> unlocks full access — buying, selling, AI tools, alerts, everything.\n\n👛 <b>Your wallet:</b>\n<code>{}</code>\n💰 <b>Balance:</b> {balance_sol:.4} SOL\n\nDeposit at least {price} (plus a little extra for network fees), then tap Subscribe below.",
                user.active().pubkey
            ),
            Some(kb::subscribe_menu(&price)),
        ).await?;
        Ok(())
    }

    /// Sends the configured subscription price (self.subscription_lamports)
    /// from the user's own wallet to `fee_wallet` and,
    /// on success, extends `subscription_expires_at` by 30 days (stacking
    /// on top of remaining time if they still had some left, rather than
    /// resetting to exactly 30 days from now).
    async fn do_subscribe_payment(&self, chat_id: i64, telegram_id: i64, keypair: &Keypair) -> Result<()> {
        if self.fee_wallet.is_empty() {
            self.tg.send_html(chat_id, "⚠️ Subscriptions aren't configured yet — contact the bot operator.", Some(kb::main_only())).await?;
            return Ok(());
        }
        let dest_pubkey = match Pubkey::from_str(&self.fee_wallet) {
            Ok(p) => p,
            Err(_) => {
                self.tg.send_html(chat_id, "⚠️ Subscription destination is misconfigured — contact the bot operator.", Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        let blockhash_str = self.rpc.get_latest_blockhash().await?;
        let blockhash = solana_sdk::hash::Hash::from_str(&blockhash_str)?;

        let instruction = system_instruction::transfer(&keypair.pubkey(), &dest_pubkey, self.subscription_lamports);
        let mut tx = Transaction::new_with_payer(&[instruction], Some(&keypair.pubkey()));
        tx.sign(&[keypair], blockhash);

        let tx_bytes = bincode::serialize(&tx)?;
        let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tx_bytes);

        match self.rpc.send_raw_transaction_b64(&tx_b64).await {
            Ok(sig) => {
                let mut user = self.get_or_create_user(telegram_id)?;
                let now = crate::state::chrono_now();
                let base = if user.subscription_expires_at > now { user.subscription_expires_at } else { now };
                user.subscription_expires_at = base + 30 * 86_400;
                self.db.save_user(&user)?;
                self.tg.send_html(
                    chat_id,
                    &format!("✅ <b>Subscribed!</b>\n\n💸 {} sent.\n🔗 Tx: <code>{sig}</code>\n\nAccess renewed for 30 days.", self.subscription_sol_str()),
                    Some(kb::main_only()),
                ).await?;
            }
            Err(e) => {
                self.tg.send_html(
                    chat_id,
                    &format!("❌ Payment failed: {e}\n\nMake sure your wallet has at least {} plus a bit extra for network fees, then try again.", self.subscription_sol_str()),
                    Some(kb::main_only()),
                ).await?;
            }
        }
        Ok(())
    }

    // ---------- message dispatch ----------

    pub async fn handle_message(&self, msg: TgMessage) -> Result<()> {
        let chat_id = msg.chat.id;
        let telegram_id = match &msg.from {
            Some(u) => u.id,
            None => return Ok(()),
        };
        let text = match &msg.text {
            Some(t) => t.trim().to_string(),
            None => return Ok(()),
        };

        if text.starts_with('/') {
            return self.handle_command(chat_id, telegram_id, &text).await;
        }

        let mut user = self.get_or_create_user(telegram_id)?;

        // Security: if the user's next message was going to be a PIN or a
        // raw private key, get rid of it from Telegram's chat history right
        // now -- before we even look at whether it was correct. This is
        // deliberately unconditional (fires on wrong PINs too, and even if
        // something below errors out) so nothing sensitive is ever left
        // sitting in the conversation for someone with a stolen/unlocked
        // phone to scroll back and find. Best-effort: Telegram occasionally
        // rejects a delete (message already gone, edited, etc) -- that's
        // not worth failing the whole request over.
        if user.awaiting.expects_sensitive_input() {
            let _ = self.tg.delete_message(chat_id, msg.message_id).await;
        }

        match user.awaiting.clone() {
            Awaiting::SettingPin { pending_wallet_secret_plain_b58 } => {
                if text.len() < self.min_pin_length || !text.chars().all(|c| c.is_ascii_digit()) {
                    self.tg
                        .send_html(
                            chat_id,
                            &format!("❌ PIN must be at least {} digits, numbers only. Try again:", self.min_pin_length),
                            None,
                        )
                        .await?;
                    return Ok(());
                }
                let mut plaintext = pending_wallet_secret_plain_b58;
                let secret = self.crypto.encrypt_with_pin(&text, plaintext.as_bytes())?;
                plaintext.zeroize();
                user.active_mut().secret = secret;
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                self.tg
                    .send_html(chat_id, "✅ <b>PIN set!</b> Your wallet is now encrypted and ready to use.", None)
                    .await?;
                self.show_main(chat_id, telegram_id).await?;
            }

            Awaiting::VerifyingPinForExport => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        let key = bs58::encode(kp.to_bytes()).into_string();
                        let sent = self.tg.send_html(chat_id,
                            &format!(
                                "🔑 <b>Export Private Key</b>\n\n⚠️ <b>WARNING: Never share this with anyone!</b>\nAnyone with this key has full control of your wallet.\n\n<code>{key}</code>\n\nImport this into Phantom, Solflare or any Solana wallet. This message self-deletes in 60s — tap below to remove it sooner.",
                            ),
                            Some(kb::export_key_keyboard()),
                        ).await?;
                        if let Some(mid) = sent {
                            let tg = self.tg.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                let _ = tg.delete_message(chat_id, mid).await;
                            });
                        }
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForWithdraw { dest, amount_sol } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        let is_new = !user.known_withdraw_addresses.iter().any(|a| a == &dest);
                        if is_new {
                            user.known_withdraw_addresses.push(dest.clone());
                        }
                        self.db.save_user(&user)?;
                        self.store_session(telegram_id, &kp);
                        if is_new {
                            self.tg
                                .send_html(
                                    chat_id,
                                    "✅ PIN confirmed. This is a new withdrawal address, so sending in 30s as a safety window — message us if this wasn't you.",
                                    None,
                                )
                                .await?;
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        }
                        self.do_withdraw(chat_id, &kp, &dest, amount_sol).await?;
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForSubscribe => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        self.do_subscribe_payment(chat_id, telegram_id, &kp).await?;
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::EnteringNewPin => {
                if text.len() < self.min_pin_length || !text.chars().all(|c| c.is_ascii_digit()) {
                    self.tg
                        .send_html(
                            chat_id,
                            &format!("❌ PIN must be at least {} digits, numbers only. Try again:", self.min_pin_length),
                            None,
                        )
                        .await?;
                    return Ok(());
                }
                user.awaiting = Awaiting::VerifyingPinForChangePin { new_pin: text.clone() };
                self.db.save_user(&user)?;
                self.tg
                    .send_html(chat_id, "✅ Got it. Now enter your <b>current</b> PIN to confirm the change:", None)
                    .await?;
            }

            Awaiting::VerifyingPinForChangePin { new_pin } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                let now = crate::state::chrono_now();
                // One PIN protects every wallet slot, so changing it has
                // to re-wrap ALL of them -- rewrapping only the active
                // slot would silently leave every other wallet locked
                // under the OLD PIN, permanently unreachable the moment
                // this save completes. Rewrap into a scratch Vec first so
                // a mid-way failure (shouldn't happen -- same PIN, same
                // crypto call, per slot -- but be defensive) never leaves
                // some slots on the new PIN and others on the old one.
                let mut rewrapped = Vec::with_capacity(user.wallets.len());
                let mut failed = false;
                for slot in &user.wallets {
                    match self.crypto.rewrap_with_new_pin(&text, &new_pin, &slot.secret) {
                        Ok(fresh) => rewrapped.push(fresh),
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
                if failed || rewrapped.len() != user.wallets.len() {
                    user.pin_lockout.record_failure(now);
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "❌ Wrong current PIN. Try again, or /cancel.", None).await?;
                } else {
                    for (slot, fresh) in user.wallets.iter_mut().zip(rewrapped.into_iter()) {
                        slot.secret = fresh;
                    }
                    user.pin_lockout.record_success();
                    user.awaiting = Awaiting::None;
                    self.db.save_user(&user)?;
                    self.clear_session(telegram_id);
                    self.tg.send_html(chat_id, "✅ <b>PIN changed!</b> All of your wallets are protected by the new PIN.", Some(kb::main_only())).await?;
                }
            }

            Awaiting::EnteringImportKey => {
                match wallet::import_wallet(&text) {
                    Ok(_) => {
                        user.awaiting = Awaiting::VerifyingPinForImport { pending_key_b58: text.clone() };
                        self.db.save_user(&user)?;
                        self.tg
                            .send_html(chat_id, "🔑 Enter your PIN to confirm and encrypt this wallet:", Some(kb::cancel_to("wallet")))
                            .await?;
                    }
                    Err(e) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        self.tg.send_html(chat_id, &format!("❌ {e}\n\nMake sure you're pasting a raw Solana private key (base58), not a seed phrase."), Some(kb::main_only())).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForImport { pending_key_b58 } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                let now = crate::state::chrono_now();
                // Any existing slot's secret works to verify the PIN --
                // every wallet under this account shares the same PIN.
                match wallet::load_keypair(&self.crypto, &text, &user.active().secret) {
                    Ok(_) => {
                        user.pin_lockout.record_success();
                        if user.wallets.len() >= crate::state::MAX_WALLETS {
                            self.db.save_user(&user)?;
                            self.tg.send_html(chat_id, &format!("❌ You're at the {}-wallet limit — remove one before importing another.", crate::state::MAX_WALLETS), Some(kb::main_only())).await?;
                            return Ok(());
                        }
                        match wallet::import_encrypted_wallet(&self.crypto, &text, &pending_key_b58) {
                            Ok((pubkey, secret)) => {
                                if user.wallets.iter().any(|w| w.pubkey == pubkey) {
                                    self.db.save_user(&user)?;
                                    self.tg.send_html(chat_id, "❌ That wallet is already imported under one of your existing slots.", Some(kb::main_only())).await?;
                                    return Ok(());
                                }
                                let label = user.next_wallet_label();
                                user.wallets.push(crate::state::WalletSlot::new(label.clone(), pubkey.clone(), secret));
                                user.active_wallet = user.wallets.len() - 1;
                                user.awaiting = Awaiting::None;
                                self.db.save_user(&user)?;
                                self.clear_session(telegram_id);
                                self.tg.send_html(chat_id,
                                    &format!("✅ <b>Wallet Imported as {label}!</b>\n\n📍 <b>Address:</b> <code>{pubkey}</code>\n\nThis is now your active wallet — switch back anytime from Wallet → 🔀 Switch Wallet."),
                                    Some(kb::wallet_menu(&user)),
                                ).await?;
                            }
                            Err(e) => {
                                self.db.save_user(&user)?;
                                self.tg.send_html(chat_id, &format!("❌ {e}"), Some(kb::main_only())).await?;
                            }
                        }
                    }
                    Err(_) => {
                        user.pin_lockout.record_failure(now);
                        self.db.save_user(&user)?;
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForAddWallet { pending_pubkey, pending_wallet_secret_plain_b58 } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                let now = crate::state::chrono_now();
                // Verify against any existing slot -- same shared PIN.
                match wallet::load_keypair(&self.crypto, &text, &user.active().secret) {
                    Ok(_) => {
                        user.pin_lockout.record_success();
                        let mut plaintext = pending_wallet_secret_plain_b58;
                        let encrypt_result = self.crypto.encrypt_with_pin(&text, plaintext.as_bytes());
                        plaintext.zeroize();
                        match encrypt_result {
                            Ok(secret) => {
                                let label = user.next_wallet_label();
                                user.wallets.push(crate::state::WalletSlot::new(label.clone(), pending_pubkey.clone(), secret));
                                user.active_wallet = user.wallets.len() - 1;
                                user.awaiting = Awaiting::None;
                                self.db.save_user(&user)?;
                                self.clear_session(telegram_id);
                                self.tg.send_html(chat_id,
                                    &format!("✅ <b>New wallet {label} created!</b>\n\n📍 <b>Address:</b> <code>{pending_pubkey}</code>\n\nThis is now your active wallet — switch back anytime from Wallet → 🔀 Switch Wallet."),
                                    Some(kb::wallet_menu(&user)),
                                ).await?;
                            }
                            Err(e) => {
                                self.db.save_user(&user)?;
                                self.tg.send_html(chat_id, &format!("❌ Couldn't create wallet: {e}"), Some(kb::main_only())).await?;
                            }
                        }
                    }
                    Err(_) => {
                        user.pin_lockout.record_failure(now);
                        self.db.save_user(&user)?;
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::EnteringWithdrawAddress => {
                user.awaiting = Awaiting::None;
                let dest = text.clone();
                if Pubkey::from_str(&dest).is_err() {
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "❌ That doesn't look like a valid Solana address. Try again from the Wallet menu.", Some(kb::main_only())).await?;
                    return Ok(());
                }
                let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                if balance_sol <= 0.0005 {
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "❌ Insufficient balance to withdraw.", Some(kb::main_only())).await?;
                    return Ok(());
                }
                let amount_sol = (balance_sol - 0.0005).max(0.0); // leave a little for fees/rent
                let is_known = user.known_withdraw_addresses.iter().any(|a| a == &dest);
                user.awaiting = Awaiting::VerifyingPinForWithdraw { dest: dest.clone(), amount_sol };
                self.db.save_user(&user)?;
                if is_known {
                    self.tg.send_html(chat_id, &format!("⬆️ Withdrawing ~{amount_sol:.4} SOL.\n\nEnter your PIN to confirm:"), None).await?;
                } else {
                    self.tg.send_html(chat_id, &format!(
                        "⚠️ <b>New withdrawal address</b> — you haven't sent here before.\n\n⬆️ Withdrawing ~{amount_sol:.4} SOL to:\n<code>{dest}</code>\n\nEnter your PIN to confirm:"
                    ), None).await?;
                }
            }

            Awaiting::EnteringBuyCA => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                self.handle_buy_ca(chat_id, &text).await?;
            }

            Awaiting::EnteringSellCA => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                self.handle_sell_query(chat_id, &user, &text).await?;
            }

            Awaiting::EnteringCustomSellPercent { ca } => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                match text.trim_end_matches('%').parse::<f64>() {
                    Ok(pct) if pct > 0.0 && pct <= 100.0 => {
                        self.handle_sell_pct(chat_id, telegram_id, &ca, pct.round() as u8).await?;
                    }
                    _ => {
                        self.tg.send_html(chat_id, "❌ Invalid percentage. Enter a number between 1 and 100, e.g. <code>40</code>", Some(kb::main_only())).await?;
                    }
                }
            }

            Awaiting::EnteringRugScanCA => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                self.handle_rug_scan(chat_id, &text).await?;
            }

            Awaiting::EnteringCustomBuyAmount { ca } => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                match text.parse::<f64>() {
                    Ok(amount) if amount > 0.0 => {
                        let data = format!("buyamt_{ca}_{amount}");
                        self.handle_buy_amount(chat_id, telegram_id, &data).await?;
                    }
                    _ => {
                        self.tg.send_html(chat_id, "❌ Invalid amount. Please enter a number like <code>0.25</code>", Some(kb::main_only())).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForBuy { ca, amount_sol } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        self.store_session(telegram_id, &kp);
                        self.execute_buy(chat_id, &mut user, &kp, &ca, amount_sol).await?;
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForSell { ca, pct } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        self.store_session(telegram_id, &kp);
                        self.execute_sell(chat_id, &mut user, &kp, &ca, pct).await?;
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::EnteringCustomStakeAmount => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                match text.parse::<f64>() {
                    Ok(amount) if amount > 0.0 => {
                        let data = format!("yieldamt_{amount}");
                        self.handle_yield_amount(chat_id, telegram_id, &data).await?;
                    }
                    _ => {
                        self.tg.send_html(chat_id, "❌ Invalid amount. Please enter a number like <code>0.5</code>", Some(kb::main_only())).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForStake { amount_sol } => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        self.store_session(telegram_id, &kp);
                        self.execute_stake(chat_id, &mut user, &kp, amount_sol).await?;
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::VerifyingPinForUnstake => {
                if let Some(m) = lockout_message(&user) {
                    self.tg.send_html(chat_id, &m, None).await?;
                    return Ok(());
                }
                match self.try_pin(&mut user, &text)? {
                    Some(kp) => {
                        user.awaiting = Awaiting::None;
                        self.db.save_user(&user)?;
                        self.store_session(telegram_id, &kp);
                        self.execute_unstake(chat_id, &mut user, &kp).await?;
                    }
                    None => {
                        self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                    }
                }
            }

            Awaiting::ConfirmingReset => {
                if text.eq_ignore_ascii_case("RESET") {
                    self.db.delete_user(telegram_id)?;
                    // Recreate exactly as a first-time /start would: fresh
                    // wallet generated, awaiting a new PIN.
                    let new_user = self.get_or_create_user(telegram_id)?;
                    let Awaiting::SettingPin { .. } = new_user.awaiting else {
                        // get_or_create_user always sets this for a record
                        // that didn't previously exist -- if it somehow
                        // didn't, fail loudly rather than leave the user
                        // stuck with no wallet and no prompt.
                        return Err(anyhow!("reset: expected fresh SettingPin state, got something else"));
                    };
                    self.tg.send_html(
                        chat_id,
                        &format!(
                            "✅ <b>Account reset.</b>\n\nA new Solana wallet has been created for you:\n<code>{}</code>\n\n\
                             Choose a PIN (at least {} digits) to protect it:",
                            new_user.active().pubkey, self.min_pin_length
                        ),
                        None,
                    ).await?;
                } else {
                    user.awaiting = Awaiting::None;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "Cancelled — your existing wallet is unchanged.", Some(kb::main_only())).await?;
                }
            }

            Awaiting::None => {}
        }

        Ok(())
    }

    async fn handle_command(&self, chat_id: i64, telegram_id: i64, text: &str) -> Result<()> {
        let cmd = text.split_whitespace().next().unwrap_or("");

        if !matches!(cmd, "/start" | "/cancel" | "/help") {
            let user = self.get_or_create_user(telegram_id)?;
            let mid_pin_setup = matches!(user.awaiting, Awaiting::SettingPin { .. });
            if !mid_pin_setup && !self.has_access(&user) {
                self.send_subscribe_prompt(chat_id, &user).await?;
                return Ok(());
            }
        }

        match cmd {
            "/start" => {
                let user = self.get_or_create_user(telegram_id)?;
                if let Awaiting::SettingPin { .. } = user.awaiting {
                    self.tg.send_html(chat_id,
                        &format!(
                            "👻 <b>Welcome to Wraith</b>\n\nA real Solana wallet has been created for you:\n<code>{}</code>\n\n⚠️ <b>Important — read this once:</b>\nThis is a <b>custodial</b> wallet. Your private key is encrypted at rest using your PIN combined with a server-side secret — nobody, including the bot operator, can decrypt it without your PIN.\n\nYou can export your private key anytime via Wallet → Export Private Key and move to your own wallet.\n\nNow choose a PIN (at least {} digits) to protect your wallet:",
                            user.active().pubkey, self.min_pin_length
                        ),
                        None,
                    ).await?;
                } else {
                    self.show_main(chat_id, telegram_id).await?;
                }
            }
            "/cancel" => {
                let mut u = self.get_or_create_user(telegram_id)?;
                if let Awaiting::SettingPin { .. } = u.awaiting {
                    self.tg.send_html(chat_id, "You need to set a PIN before continuing — it's required to protect your wallet.", None).await?;
                    return Ok(());
                }
                u.awaiting = Awaiting::None;
                self.db.save_user(&u)?;
                self.tg.send_html(chat_id, "Cancelled.", Some(kb::main_only())).await?;
            }
            "/help" => {
                self.tg.send_html(chat_id,
                    "👻 <b>Wraith Commands</b>\n\n/start — Main menu\n/buy — Buy a token (CA or name)\n/sell — Sell a token\n/positions — Open positions\n/pnl — P/L summary\n/gemscan — AI Gem Scanner\n/balance — Check balance\n/cancel — Cancel current action\n/help — This message",
                    None,
                ).await?;
            }
            "/buy" => {
                let mut u = self.get_or_create_user(telegram_id)?;
                u.awaiting = Awaiting::EnteringBuyCA;
                self.db.save_user(&u)?;
                self.tg.send_html(chat_id, "🟢 <b>Buy Token</b>\n\nPaste the contract address (CA), or type the coin's name/symbol:", Some(kb::cancel_to("main"))).await?;
            }
            "/sell" => {
                self.start_sell_flow(chat_id, telegram_id).await?;
            }
            "/balance" => {
                let user = self.get_or_create_user(telegram_id)?;
                let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                self.tg.send_html(chat_id, &format!("💰 <b>Balance:</b> {balance_sol:.4} SOL\n👛 <code>{}</code>", user.active().pubkey), None).await?;
            }
            "/pnl" => {
                self.handle_pnl(chat_id, telegram_id).await?;
            }
            "/positions" => {
                self.handle_positions(chat_id, telegram_id).await?;
            }
            "/gemscan" | "/scan" => {
                self.handle_gem_scan(chat_id).await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn handle_callback(&self, callback_id: &str, data: &str, chat_id: i64, telegram_id: i64, message_id: Option<i64>) -> Result<()> {
        self.tg.answer_callback(callback_id, None).await.ok();
        let mut user = self.get_or_create_user(telegram_id)?;

        if let Awaiting::SettingPin { .. } = user.awaiting {
            self.tg.send_html(chat_id, &format!("🔒 Please finish setting your PIN first (at least {} digits):", self.min_pin_length), None).await?;
            return Ok(());
        }

        if !self.has_access(&user) && !matches!(data, "wallet" | "main" | "refresh" | "subscribe" | "del_msg") {
            self.send_subscribe_prompt(chat_id, &user).await?;
            return Ok(());
        }

        match data {
            "del_msg" => {
                if let Some(mid) = message_id {
                    self.tg.delete_message(chat_id, mid).await.ok();
                }
            }
            "main" | "refresh" => {
                self.show_main(chat_id, telegram_id).await?;
            }
            "subscribe" => {
                if self.has_access(&user) {
                    self.tg.send_html(chat_id, "✅ You're already subscribed.", Some(kb::main_only())).await?;
                } else {
                    user.awaiting = Awaiting::VerifyingPinForSubscribe;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, &format!("🔑 Enter your PIN to confirm your {}/month payment:", self.subscription_sol_str()), None).await?;
                }
            }
            "wallet" => {
                let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                let label = user.active().label.clone();
                self.tg.send_html(chat_id,
                    &format!("💰 <b>Wallet {label}</b>\n\n📍 <b>Address:</b>\n<code>{}</code>\n\n💎 <b>Balance:</b> {balance_sol:.4} SOL\n\nSend SOL to this address to deposit.", user.active().pubkey),
                    Some(kb::wallet_menu(&user)),
                ).await?;
            }
            "wallet_switch" => {
                self.tg.send_html(chat_id,
                    &format!("🔀 <b>Your Wallets</b> ({}/{})\n\nTap one to make it active, or add another below.", user.wallets.len(), crate::state::MAX_WALLETS),
                    Some(kb::wallet_switcher(&user)),
                ).await?;
            }
            other if other.starts_with("walletsel_") => {
                if let Ok(i) = other.trim_start_matches("walletsel_").parse::<usize>() {
                    if i < user.wallets.len() {
                        user.active_wallet = i;
                        self.db.save_user(&user)?;
                        // The cached trading-session keypair (if any) is
                        // for the PREVIOUS active wallet -- keep using it
                        // after a switch would sign with the wrong
                        // wallet's key. Drop it; next trade re-prompts
                        // for the PIN, same as after any other wallet
                        // change.
                        self.clear_session(telegram_id);
                        let label = user.active().label.clone();
                        self.tg.send_html(chat_id, &format!("✅ Switched to <b>{label}</b>."), Some(kb::wallet_menu(&user))).await?;
                    }
                }
            }
            "wallet_add" => {
                if user.wallets.len() >= crate::state::MAX_WALLETS {
                    self.tg.send_html(chat_id, &format!("❌ You're at the {}-wallet limit.", crate::state::MAX_WALLETS), Some(kb::wallet_menu(&user))).await?;
                } else {
                    self.tg.send_html(chat_id, "➕ <b>Add Wallet</b>\n\nGenerate a brand new Wraith wallet, or import one you already have:", Some(kb::add_wallet_menu())).await?;
                }
            }
            "wallet_add_new" => {
                if user.wallets.len() >= crate::state::MAX_WALLETS {
                    self.tg.send_html(chat_id, &format!("❌ You're at the {}-wallet limit.", crate::state::MAX_WALLETS), Some(kb::wallet_menu(&user))).await?;
                } else {
                    let wallet = wallet::create_wallet();
                    user.awaiting = Awaiting::VerifyingPinForAddWallet {
                        pending_pubkey: wallet.address.clone(),
                        pending_wallet_secret_plain_b58: wallet.private_key_base58.clone(),
                    };
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id,
                        &format!("🆕 New wallet address:\n<code>{}</code>\n\n🔑 Enter your PIN to confirm and encrypt it (same PIN as your other wallets):", wallet.address),
                        Some(kb::cancel_to("wallet")),
                    ).await?;
                }
            }
            "buy" => {
                user.awaiting = Awaiting::EnteringBuyCA;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "🟢 <b>Buy Token</b>\n\nPaste the contract address (CA), or type the coin's name/symbol:", Some(kb::cancel_to("main"))).await?;
            }
            "sell" => {
                self.start_sell_flow(chat_id, telegram_id).await?;
            }
            "positions" => {
                self.handle_positions(chat_id, telegram_id).await?;
            }
            "withdraw" => {
                let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                if balance_sol <= 0.0005 {
                    self.tg.send_html(chat_id, "❌ Insufficient balance to withdraw.", Some(kb::main_only())).await?;
                } else {
                    user.awaiting = Awaiting::EnteringWithdrawAddress;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "⬆️ <b>Withdraw</b>\n\nSend the destination wallet address:", Some(kb::cancel_to("wallet"))).await?;
                }
            }
            "export_key" => {
                user.awaiting = Awaiting::VerifyingPinForExport;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "🔑 Enter your PIN to export your private key:", None).await?;
            }
            "import_wallet" => {
                if user.wallets.len() >= crate::state::MAX_WALLETS {
                    self.tg.send_html(chat_id, &format!("❌ You're at the {}-wallet limit — remove one before importing another.", crate::state::MAX_WALLETS), Some(kb::wallet_menu(&user))).await?;
                } else {
                    user.awaiting = Awaiting::EnteringImportKey;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id,
                        "📥 <b>Import Wallet</b>\n\n⚠️ Only do this on a trusted device. This ADDS the imported wallet as a new sub-account (your existing wallets are untouched) and makes it active.\n\nSend your Solana private key (base58):",
                        Some(kb::cancel_to("wallet")),
                    ).await?;
                }
            }
            "ai_tools" => {
                self.tg.send_html(chat_id, "🤖 <b>AI Tools</b>", Some(kb::ai_tools_menu())).await?;
            }
            "rug_scan" => {
                user.awaiting = Awaiting::EnteringRugScanCA;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "🔍 Paste a CA to scan:", Some(kb::cancel_to("ai_tools"))).await?;
            }
            "trade_signals" => {
                self.handle_trade_signals(chat_id).await?;
            }
            "pnl" => {
                self.handle_pnl(chat_id, telegram_id).await?;
            }
            "gem_scan" => {
                self.handle_gem_scan(chat_id).await?;
            }
            "settings" => {
                self.tg.send_html(chat_id, "⚙️ <b>Settings</b>", Some(kb::settings_menu(user.gem_alerts, user.yield_auto_enabled))).await?;
            }
            "toggle_gem_alerts" => {
                user.gem_alerts = !user.gem_alerts;
                self.db.save_user(&user)?;
                let status = if user.gem_alerts { "🔔 Gem alerts <b>enabled</b> — you'll be notified when new gems are found." } else { "🔕 Gem alerts <b>disabled</b> — you won't receive gem notifications." };
                self.tg.send_html(chat_id, status, Some(kb::settings_menu(user.gem_alerts, user.yield_auto_enabled))).await?;
            }
            "toggle_auto_yield" => {
                user.yield_auto_enabled = !user.yield_auto_enabled;
                self.db.save_user(&user)?;
                let status = if user.yield_auto_enabled {
                    "🌾 <b>Auto-Yield enabled</b> — from now on, idle SOL in your active wallet gets staked into JitoSOL automatically (whenever the bot already has your trading session unlocked), and auto-unstaked the instant you need it for a buy. Wraith still only ever takes 10% of the gain, never your principal.\n\n<i>Note: this only runs while your 15-min trading session is unlocked -- Wraith never stores your PIN, so it can't act while you're fully logged out.</i>"
                } else {
                    "🌾 <b>Auto-Yield disabled</b> — your SOL will stay exactly where it is. Anything already staked stays staked until you manually unstake it from 🌱 Yield."
                };
                self.tg.send_html(chat_id, status, Some(kb::settings_menu(user.gem_alerts, user.yield_auto_enabled))).await?;
                // If we already have a live session (they just unlocked
                // trading recently), sweep right away instead of making
                // them wait for the next periodic pass or their next buy.
                if user.yield_auto_enabled {
                    if let Some(kp) = self.session_keypair(telegram_id) {
                        self.maybe_auto_sweep_idle(chat_id, &mut user, &kp).await;
                    }
                }
            }
            "change_pin" => {
                user.awaiting = Awaiting::EnteringNewPin;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, &format!("🔑 Enter your <b>new</b> PIN (at least {} digits):", self.min_pin_length), None).await?;
            }
            "reset_account" => {
                user.awaiting = Awaiting::ConfirmingReset;
                self.db.save_user(&user)?;
                self.tg.send_html(
                    chat_id,
                    "⚠️ <b>Reset Account</b>\n\n\
                     This is for when you've lost your PIN and can't get back into your current wallet.\n\n\
                     Resetting will generate a <b>brand new wallet</b> for you here in Wraith. Your current wallet address still exists on-chain and is untouched — but Wraith will no longer have any way to access it, since that requires the PIN you no longer have.\n\n\
                     • If you previously exported/saved that wallet's private key, you can still import it into any external wallet (Phantom, Backpack, etc.) at any time — nothing is lost.\n\
                     • If you never saved it, any funds in that wallet are permanently inaccessible. This is not something Wraith or its operator can undo or override — nobody but you ever held the PIN.\n\n\
                     Type <b>RESET</b> to confirm, or anything else to cancel.",
                    Some(kb::cancel_to("main")),
                ).await?;
            }
            "slippage" => {
                self.tg.send_html(chat_id, &format!("📊 Current slippage: {:.1}%\n\nSelect:", user.slippage_bps as f64 / 100.0), Some(kb::slippage_menu())).await?;
            }
            "referral" => {
                self.tg.send_html(chat_id, "👥 <b>Referral Program</b>\n\nComing soon.", Some(kb::main_only())).await?;
            }
            "yield" => {
                self.show_yield_menu(chat_id, &user).await?;
            }
            "yield_unstake" => {
                if let Some(kp) = self.session_keypair(telegram_id) {
                    self.execute_unstake(chat_id, &mut user, &kp).await?;
                } else {
                    if let Some(m) = lockout_message(&user) {
                        self.tg.send_html(chat_id, &m, Some(kb::main_only())).await?;
                        return Ok(());
                    }
                    user.awaiting = Awaiting::VerifyingPinForUnstake;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "🔑 Enter your PIN to confirm unstaking:", Some(kb::cancel_to("main"))).await?;
                }
            }
            other if other.starts_with("yieldamt_") => {
                if other.ends_with("_custom") {
                    user.awaiting = Awaiting::EnteringCustomStakeAmount;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "✏️ Enter the amount of SOL you want to stake (e.g. <code>2.5</code>):", Some(kb::cancel_to("main"))).await?;
                } else {
                    self.handle_yield_amount(chat_id, telegram_id, other).await?;
                }
            }
            other if other.starts_with("bcp_") => {
                // Quick buy from gem/pump alerts. Uses a short "bcp_" prefix
                // instead of "buyamt_<ca>_custom_prompt" -- that longer form
                // could exceed Telegram's 64-byte callback_data limit once a
                // full 44-char mint address was embedded (BUTTON_DATA_INVALID).
                let ca = other.trim_start_matches("bcp_");
                user.awaiting = Awaiting::EnteringCustomBuyAmount { ca: ca.to_string() };
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "✏️ How much SOL do you want to spend? (e.g. <code>0.5</code>):", Some(kb::cancel_to("main"))).await?;
            }
            other if other.starts_with("buyamt_") => {
                if other.ends_with("_custom") {
                    let ca = other.trim_start_matches("buyamt_").trim_end_matches("_custom");
                    user.awaiting = Awaiting::EnteringCustomBuyAmount { ca: ca.to_string() };
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "✏️ Enter the amount of SOL you want to spend (e.g. <code>0.25</code>):", Some(kb::cancel_to("main"))).await?;
                } else {
                    self.handle_buy_amount(chat_id, telegram_id, other).await?;
                }
            }
            other if other.starts_with("sellsel_") => {
                let ca = other.trim_start_matches("sellsel_");
                let label = user.active().positions.iter().find(|p| p.mint == ca).map(|p| p.symbol.clone()).unwrap_or_else(|| short_wallet(ca));
                self.tg.send_html(chat_id, &format!("🔴 <b>Sell {label}</b>\n\nHow much do you want to sell?"), Some(kb::sell_percent_menu(ca))).await?;
            }
            other if other.starts_with("sellpct_") => {
                let rest = other.trim_start_matches("sellpct_");
                let Some((ca, pct_str)) = rest.rsplit_once('_') else { return Ok(()); };
                if pct_str == "custom" {
                    user.awaiting = Awaiting::EnteringCustomSellPercent { ca: ca.to_string() };
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "✏️ What percentage do you want to sell? (1-100, e.g. <code>40</code>):", Some(kb::cancel_to("main"))).await?;
                } else if let Ok(pct) = pct_str.parse::<u8>() {
                    self.handle_sell_pct(chat_id, telegram_id, ca, pct).await?;
                }
            }
            other if other.starts_with("slip_") => {
                if let Ok(bps) = other.trim_start_matches("slip_").parse::<u32>() {
                    user.slippage_bps = bps;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, &format!("✅ Slippage set to {:.1}%", bps as f64 / 100.0), Some(kb::main_only())).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// True if `s` looks like a Solana base58 public key (a CA), rather
    /// than a coin name/symbol someone typed to search for. Real addresses
    /// are base58, 32-44 chars, and decode to a valid Pubkey; anything else
    /// (spaces, punctuation, wrong length) is treated as a search query.
    fn looks_like_ca(s: &str) -> bool {
        let s = s.trim();
        (32..=44).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric()) && Pubkey::from_str(s).is_ok()
    }

    /// Resolves free-typed user input (a pasted CA, or a coin name/symbol)
    /// to a concrete contract address. If it already looks like a CA, use
    /// it as-is. Otherwise searches DexScreener by name/symbol and picks
    /// the highest-liquidity match, since that's overwhelmingly the "real"
    /// token when a name is shared by low-liquidity scam clones.
    async fn resolve_token_query(&self, chat_id: i64, input: &str) -> Result<Option<String>> {
        let input = input.trim();
        if Self::looks_like_ca(input) {
            return Ok(Some(input.to_string()));
        }
        let results = dexscreener::search_tokens(input).await.unwrap_or_default();
        let Some(top) = results.first() else {
            self.tg.send_html(
                chat_id,
                &format!(
                    "❌ Couldn't find a token matching \"{}\". Try pasting the contract address (CA) instead.",
                    crate::telegram::escape_html(input)
                ),
                Some(kb::main_only()),
            ).await?;
            return Ok(None);
        };
        let ca = top["baseToken"]["address"].as_str().unwrap_or("").to_string();
        if ca.is_empty() {
            return Ok(None);
        }
        let name = crate::telegram::escape_html(top["baseToken"]["name"].as_str().unwrap_or("Unknown"));
        let symbol = crate::telegram::escape_html(top["baseToken"]["symbol"].as_str().unwrap_or("???"));
        self.tg.send_html(
            chat_id,
            &format!("🔎 Matched \"{}\" → <b>{name} (${symbol})</b>\n<code>{ca}</code>", crate::telegram::escape_html(input)),
            None,
        ).await?;
        Ok(Some(ca))
    }

    /// Same idea as `resolve_token_query`, but for sells: checks the
    /// user's own open positions by symbol first (so "sell bonk" always
    /// hits the exact mint they actually bought, not a same-named clone
    /// DexScreener happens to rank higher), then falls back to the same
    /// name search.
    async fn resolve_sell_query(&self, chat_id: i64, user: &UserRecord, input: &str) -> Result<Option<String>> {
        let input = input.trim();
        if Self::looks_like_ca(input) {
            return Ok(Some(input.to_string()));
        }
        if let Some(p) = user.active().positions.iter().find(|p| p.symbol.eq_ignore_ascii_case(input)) {
            return Ok(Some(p.mint.clone()));
        }
        self.resolve_token_query(chat_id, input).await
    }

    /// Entry point for the "Sell" button / `/sell` command: shows a
    /// one-tap picker of the user's current holdings if they have any,
    /// otherwise falls back to asking for a CA/name to type.
    async fn start_sell_flow(&self, chat_id: i64, telegram_id: i64) -> Result<()> {
        let mut user = self.get_or_create_user(telegram_id)?;
        if user.active().positions.is_empty() {
            user.awaiting = Awaiting::EnteringSellCA;
            self.db.save_user(&user)?;
            self.tg.send_html(chat_id, "🔴 <b>Sell Token</b>\n\nYou don't have any tracked positions, but you can still sell any token in your wallet. Paste the CA, or type the coin's name/symbol:", Some(kb::cancel_to("main"))).await?;
        } else {
            user.awaiting = Awaiting::EnteringSellCA;
            self.db.save_user(&user)?;
            self.tg.send_html(chat_id, "🔴 <b>Sell Token</b>\n\nPick a position below, or type a CA/coin name:", Some(kb::position_list(&user.active().positions))).await?;
        }
        Ok(())
    }

    /// Handles typed input (CA or coin name) while `Awaiting::EnteringSellCA`
    /// -- resolves it, then shows the sell-percentage keyboard rather than
    /// selling immediately.
    async fn handle_sell_query(&self, chat_id: i64, user: &UserRecord, input: &str) -> Result<()> {
        let Some(ca) = self.resolve_sell_query(chat_id, user, input).await? else { return Ok(()); };
        let label = user.active().positions.iter().find(|p| p.mint == ca).map(|p| p.symbol.clone()).unwrap_or_else(|| short_wallet(&ca));
        self.tg.send_html(chat_id, &format!("🔴 <b>Sell {label}</b>\n\nHow much do you want to sell?"), Some(kb::sell_percent_menu(&ca))).await?;
        Ok(())
    }

    /// Kicks off (or PIN-gates) a sell for `pct`% of the user's current
    /// balance of `ca`, once a percentage has been chosen.
    async fn handle_sell_pct(&self, chat_id: i64, telegram_id: i64, ca: &str, pct: u8) -> Result<()> {
        let mut user = self.get_or_create_user(telegram_id)?;
        if let Some(kp) = self.session_keypair(telegram_id) {
            self.execute_sell(chat_id, &mut user, &kp, ca, pct).await?;
        } else {
            if let Some(m) = lockout_message(&user) {
                self.tg.send_html(chat_id, &m, Some(kb::main_only())).await?;
                return Ok(());
            }
            user.awaiting = Awaiting::VerifyingPinForSell { ca: ca.to_string(), pct };
            self.db.save_user(&user)?;
            self.tg.send_html(chat_id, "🔑 Enter your PIN to unlock trading (stays unlocked for 15 minutes):", Some(kb::cancel_to("main"))).await?;
        }
        Ok(())
    }

    /// Shared body for the "📊 Positions" button and the `/positions`
    /// command.
    async fn handle_positions(&self, chat_id: i64, telegram_id: i64) -> Result<()> {
        let user = self.get_or_create_user(telegram_id)?;
        if user.active().positions.is_empty() {
            self.tg.send_html(chat_id, "📊 <b>Open Positions</b>\n\nNo open positions yet.", Some(kb::main_only())).await?;
            return Ok(());
        }
        self.tg.send_html(chat_id, "📊 Fetching live prices...", None).await?;

        let sol_handle = tokio::spawn(async {
            dexscreener::get_token_pair(SOL_MINT)
                .await
                .ok()
                .flatten()
                .and_then(|p| p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()))
                .unwrap_or(0.0)
        });

        let mut set = tokio::task::JoinSet::new();
        for (i, p) in user.active().positions.iter().enumerate() {
            let mint = p.mint.clone();
            set.spawn(async move {
                let price = dexscreener::get_token_pair(&mint)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|pair| pair["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()));
                (i, price)
            });
        }
        let mut prices: Vec<Option<f64>> = vec![None; user.active().positions.len()];
        while let Some(res) = set.join_next().await {
            if let Ok((i, price)) = res {
                prices[i] = price;
            }
        }
        let sol_price_usd = sol_handle.await.unwrap_or(0.0);

        let mut msg = "📊 <b>Open Positions</b>\n\n".to_string();
        for (i, p) in user.active().positions.iter().enumerate() {
            let current_price_usd = prices[i];

            match current_price_usd {
                Some(cur) if p.entry_price_usd > 0.0 => {
                    let pct = (cur / p.entry_price_usd - 1.0) * 100.0;
                    let entry_value_usd = p.tokens_received_est * p.entry_price_usd;
                    let current_value_usd = p.tokens_received_est * cur;
                    let pl_usd = current_value_usd - entry_value_usd;
                    let pl_sol = if sol_price_usd > 0.0 { pl_usd / sol_price_usd } else { 0.0 };
                    let arrow = if pct >= 0.0 { "🟢" } else { "🔴" };
                    msg += &format!(
                        "{arrow} <b>{}</b>\n   Entry: ${:.8} → Now: ${:.8} ({pct:+.1}%)\n   P/L: {pl_sol:+.4} SOL (${pl_usd:+.2})\n\n",
                        p.symbol, p.entry_price_usd, cur
                    );
                }
                _ => {
                    msg += &format!(
                        "⚪ <b>{}</b> — spent {:.4} SOL, ~{:.2} tokens (live price unavailable)\n\n",
                        p.symbol, p.sol_spent, p.tokens_received_est
                    );
                }
            }
        }
        self.tg.send_html(chat_id, &msg, Some(kb::main_only())).await?;
        Ok(())
    }

    async fn handle_buy_ca(&self, chat_id: i64, input: &str) -> Result<()> {
        let Some(ca) = self.resolve_token_query(chat_id, input).await? else { return Ok(()); };
        let ca = ca.as_str();
        self.tg.send_html(chat_id, "🔍 Scanning token...", None).await?;
        let pair = match dexscreener::get_token_pair(ca).await {
            Ok(Some(p)) => p,
            _ => {
                self.tg.send_html(chat_id, "❌ Token not found. Check the CA and try again.", Some(kb::cancel_to("main"))).await?;
                return Ok(());
            }
        };
        let mut a = dexscreener::analyze(&pair);

        // Layer the same on-chain checks the AI Gem Scanner uses (mint/
        // freeze authority, holder concentration) so a manually-pasted CA
        // gets the same scrutiny as an auto-surfaced gem, not just the
        // DexScreener-only market heuristics.
        if let Ok((mint_ok, freeze_ok)) = self.rpc.get_mint_authority_status(ca).await {
            if mint_ok && freeze_ok {
                a.good.insert(0, "✅ Mint & freeze authority renounced".to_string());
            } else {
                a.score = (a.score - 25).max(0);
                a.flags.insert(0, "🚨 Mint/freeze authority still active — creator retains rug control".to_string());
            }
        }
        if let Ok(Some(conc)) = self.rpc.get_top10_concentration_pct(ca).await {
            if conc > 70.0 {
                a.score = (a.score - 15).max(0);
                a.flags.push(format!("🚨 Top 10 wallets hold ~{conc:.0}% of supply"));
            } else if conc < 30.0 {
                a.good.push(format!("✅ Reasonably distributed (top 10 ~{conc:.0}%)"));
            }
        }
        let (risk_level, risk_emoji) = if a.score >= 75 {
            ("SAFE", "✅")
        } else if a.score >= 50 {
            ("MODERATE RISK", "⚠️")
        } else if a.score >= 25 {
            ("HIGH RISK", "🔴")
        } else {
            ("LIKELY RUG", "🚨")
        };

        let name = crate::telegram::escape_html(pair["baseToken"]["name"].as_str().unwrap_or("Unknown"));
        let symbol = crate::telegram::escape_html(pair["baseToken"]["symbol"].as_str().unwrap_or("???"));
        let mc = pair["fdv"].as_f64().map(fmt_usd).unwrap_or_else(|| "N/A".to_string());
        let liq = pair["liquidity"]["usd"].as_f64().map(fmt_usd).unwrap_or_else(|| "N/A".to_string());
        let price = pair["priceUsd"].as_str().unwrap_or("N/A");

        let flags = if a.flags.is_empty() { String::new() } else { format!("\n🚩 <b>Red Flags:</b>\n{}", a.flags.join("\n")) };
        let good = if a.good.is_empty() { String::new() } else { format!("\n💚 <b>Positives:</b>\n{}", a.good.join("\n")) };

        self.tg.send_html(chat_id, &format!(
            "🔍 <b>{name} ({symbol})</b>\n📋 <code>{ca}</code>\n\n💎 MC: {mc}\n💧 Liq: {liq}\n💵 Price: ${price}\n\n{} <b>Risk: {}/100 — {}</b>{flags}{good}\n\nSelect buy amount:",
            risk_emoji, a.score, risk_level
        ), Some(kb::buy_amounts(ca))).await?;
        Ok(())
    }

    async fn handle_buy_amount(&self, chat_id: i64, telegram_id: i64, data: &str) -> Result<()> {
        // format: buyamt_<ca>_<amount>
        let rest = data.trim_start_matches("buyamt_");
        let (ca, amt_str) = match rest.rsplit_once('_') {
            Some(pair) => pair,
            None => return Ok(()),
        };
        let ca = ca.to_string();
        let amount_sol: f64 = amt_str.parse().unwrap_or(0.0);
        let mut user = self.get_or_create_user(telegram_id)?;

        let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
        // If Auto-Yield is on, don't reject here on liquid balance alone
        // -- execute_buy checks again and auto-unstakes to cover the gap
        // if the user has staked JitoSOL that can. If they don't have
        // enough even after that, the swap itself will fail cleanly with
        // its own clear error rather than this needing to predict it.
        if balance_sol < amount_sol && !user.yield_auto_enabled {
            self.tg.send_html(chat_id, &format!("❌ Insufficient balance. You have {balance_sol:.4} SOL, need {amount_sol} SOL."), Some(kb::main_only())).await?;
            return Ok(());
        }

        if let Some(kp) = self.session_keypair(telegram_id) {
            self.execute_buy(chat_id, &mut user, &kp, &ca, amount_sol).await?;
        } else {
            if let Some(m) = lockout_message(&user) {
                self.tg.send_html(chat_id, &m, Some(kb::main_only())).await?;
                return Ok(());
            }
            user.awaiting = Awaiting::VerifyingPinForBuy { ca, amount_sol };
            self.db.save_user(&user)?;
            self.tg.send_html(chat_id, "🔑 Enter your PIN to unlock trading (stays unlocked for 15 minutes):", Some(kb::cancel_to("main"))).await?;
        }
        Ok(())
    }

    /// Shows current yield status (staked value, principal, unrealized
    /// gain if any) plus the stake/unstake keyboard. The "current value"
    /// figure is a live Jupiter quote for the user's full JitoSOL
    /// balance -- an estimate, same as any other quote-based price shown
    /// elsewhere in the bot, not a guaranteed execution price.
    async fn show_yield_menu(&self, chat_id: i64, user: &UserRecord) -> Result<()> {
        let (jito_raw, _decimals) = self.rpc.get_token_balance(&user.active().pubkey, JITO_SOL_MINT).await.unwrap_or((0, 9));
        let principal_sol = user.active().yield_principal_lamports as f64 / LAMPORTS_PER_SOL;

        if jito_raw == 0 {
            self.tg.send_html(
                chat_id,
                "🌱 <b>Yield (JitoSOL staking)</b>\n\nStake idle SOL into JitoSOL and earn Solana staking rewards automatically -- no separate claim step, the value just grows.\n\nWraith takes a cut ONLY of the gains when you unstake -- never your principal.\n\nHow much do you want to stake?",
                Some(kb::yield_menu(false)),
            ).await?;
            return Ok(());
        }

        let est_value_sol = match self.jup.get_quote(JITO_SOL_MINT, SOL_MINT, jito_raw, 100).await {
            Ok(q) => out_amount(&q).unwrap_or(0) as f64 / LAMPORTS_PER_SOL,
            Err(_) => 0.0,
        };
        let gain_sol = (est_value_sol - principal_sol).max(0.0);
        let fee_pct = self.yield_fee_bps as f64 / 100.0;

        self.tg.send_html(
            chat_id,
            &format!(
                "🌱 <b>Yield (JitoSOL staking)</b>\n\n💰 Principal staked: {principal_sol:.4} SOL\n📈 Current est. value: {est_value_sol:.4} SOL\n✨ Unrealized gain: {gain_sol:.4} SOL\n\nUnstaking takes {fee_pct:.1}% of the gain only -- your principal is never touched. Stake more, or unstake everything:"
            ),
            Some(kb::yield_menu(true)),
        ).await?;
        Ok(())
    }

    async fn handle_yield_amount(&self, chat_id: i64, telegram_id: i64, data: &str) -> Result<()> {
        // format: yieldamt_<amount>
        let amount_sol: f64 = data.trim_start_matches("yieldamt_").parse().unwrap_or(0.0);
        if amount_sol <= 0.0 {
            self.tg.send_html(chat_id, "❌ Invalid amount.", Some(kb::main_only())).await?;
            return Ok(());
        }
        let mut user = self.get_or_create_user(telegram_id)?;

        // Leave a small buffer beyond the stake amount for network/priority
        // fees on the swap itself, same margin do_withdraw already uses
        // for the same reason.
        let balance_sol = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
        if balance_sol < amount_sol + 0.01 {
            self.tg.send_html(chat_id, &format!("❌ Insufficient balance. You have {balance_sol:.4} SOL, need at least {:.4} SOL ({amount_sol} SOL to stake + network fees).", amount_sol + 0.01), Some(kb::main_only())).await?;
            return Ok(());
        }

        if let Some(kp) = self.session_keypair(telegram_id) {
            self.execute_stake(chat_id, &mut user, &kp, amount_sol).await?;
        } else {
            if let Some(m) = lockout_message(&user) {
                self.tg.send_html(chat_id, &m, Some(kb::main_only())).await?;
                return Ok(());
            }
            user.awaiting = Awaiting::VerifyingPinForStake { amount_sol };
            self.db.save_user(&user)?;
            self.tg.send_html(chat_id, "🔑 Enter your PIN to unlock trading (stays unlocked for 15 minutes):", Some(kb::cancel_to("main"))).await?;
        }
        Ok(())
    }

    /// Swaps `amount_sol` of the user's own SOL into JitoSOL. No platform
    /// trading fee applies here (fee_wallet="") -- this is the user
    /// moving their own money into yield, not a trade; the only fee on
    /// this feature is the gains-only cut taken in execute_unstake.
    async fn execute_stake(&self, chat_id: i64, user: &mut UserRecord, keypair: &Keypair, amount_sol: f64) -> Result<()> {
        self.tg.send_html(chat_id, "⚡ Getting quote and staking...", None).await?;

        let lamports = (amount_sol * LAMPORTS_PER_SOL) as u64;
        let quote = match self.jup.get_quote(SOL_MINT, JITO_SOL_MINT, lamports, 100).await {
            Ok(q) => q,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't get a stake quote: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        match self.sign_and_send_swap(keypair, &user.active().pubkey, &quote, "").await {
            Ok(sig) => {
                let new_principal = user.active().yield_principal_lamports.saturating_add(lamports);
                user.active_mut().yield_principal_lamports = new_principal;
                self.db.save_user(user)?;
                self.tg.send_html(chat_id, &format!(
                    "✅ <b>Staked</b>\n\n💸 {amount_sol} SOL -> JitoSOL\n🔗 Tx: <code>{sig}</code>\n\nYour rewards accrue automatically -- check 🌱 Yield anytime to see current value."
                ), Some(kb::main_only())).await?;
            }
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Stake failed: {e}"), Some(kb::main_only())).await?;
            }
        }
        Ok(())
    }

    /// Swaps the user's ENTIRE JitoSOL balance back to SOL, then -- only
    /// if that came back to MORE than the tracked principal (an actual
    /// gain) -- sends `yield_fee_bps` of just that gain to the platform
    /// fee wallet as a second, separate transaction. Principal is never
    /// touched: if the estimated value came back at or below principal
    /// (e.g. an extreme slashing scenario, or a quote that moved against
    /// the user), no fee is taken at all.
    async fn execute_unstake(&self, chat_id: i64, user: &mut UserRecord, keypair: &Keypair) -> Result<()> {
        let (jito_raw, _decimals) = match self.rpc.get_token_balance(&user.active().pubkey, JITO_SOL_MINT).await {
            Ok(v) => v,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't check your JitoSOL balance: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };
        if jito_raw == 0 {
            self.tg.send_html(chat_id, "❌ You don't have anything staked.", Some(kb::main_only())).await?;
            return Ok(());
        }

        self.tg.send_html(chat_id, "⚡ Getting quote and unstaking...", None).await?;

        let quote = match self.jup.get_quote(JITO_SOL_MINT, SOL_MINT, jito_raw, 100).await {
            Ok(q) => q,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't get an unstake quote: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };
        let out_lamports = out_amount(&quote).unwrap_or(0);

        let sig = match self.sign_and_send_swap(keypair, &user.active().pubkey, &quote, "").await {
            Ok(sig) => sig,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Unstake failed: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        let principal_lamports = user.active().yield_principal_lamports;
        let gain_lamports = out_lamports.saturating_sub(principal_lamports);
        let mut fee_note = String::new();

        if gain_lamports > 0 && !self.fee_wallet.is_empty() {
            let fee_lamports = ((gain_lamports as u128) * self.yield_fee_bps as u128 / 10_000) as u64;
            if fee_lamports > 0 {
                if let Ok(dest_pubkey) = Pubkey::from_str(&self.fee_wallet) {
                    match self.rpc.get_latest_blockhash().await {
                        Ok(blockhash_str) => {
                            if let Ok(blockhash) = solana_sdk::hash::Hash::from_str(&blockhash_str) {
                                let instruction = system_instruction::transfer(&keypair.pubkey(), &dest_pubkey, fee_lamports);
                                let mut tx = Transaction::new_with_payer(&[instruction], Some(&keypair.pubkey()));
                                tx.sign(&[keypair], blockhash);
                                if let Ok(tx_bytes) = bincode::serialize(&tx) {
                                    let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tx_bytes);
                                    match self.rpc.send_raw_transaction_b64(&tx_b64).await {
                                        Ok(_) => {
                                            let fee_sol = fee_lamports as f64 / LAMPORTS_PER_SOL;
                                            let fee_pct = self.yield_fee_bps as f64 / 100.0;
                                            fee_note = format!("\n💵 Yield fee ({fee_pct:.1}% of gain): {fee_sol:.4} SOL");
                                        }
                                        Err(e) => {
                                            eprintln!("⚠️ Yield fee transfer failed after successful unstake (principal+gains still fully belong to user): {e}");
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => eprintln!("⚠️ Couldn't fetch blockhash for yield fee transfer: {e}"),
                    }
                }
            }
        }

        user.active_mut().yield_principal_lamports = 0;
        self.db.save_user(user)?;

        let out_sol = out_lamports as f64 / LAMPORTS_PER_SOL;
        let principal_sol = principal_lamports as f64 / LAMPORTS_PER_SOL;
        let gain_sol = gain_lamports as f64 / LAMPORTS_PER_SOL;
        self.tg.send_html(chat_id, &format!(
            "✅ <b>Unstaked</b>\n\n💰 Received: {out_sol:.4} SOL\n📥 Principal: {principal_sol:.4} SOL\n✨ Gain: {gain_sol:.4} SOL{fee_note}\n🔗 Tx: <code>{sig}</code>"
        ), Some(kb::main_only())).await?;
        Ok(())
    }

    /// Auto-Yield's "stake idle SOL" half. Only ever called at a moment
    /// we already have a live decrypted `keypair` in hand (right after a
    /// sell, right when the toggle is flipped on with a session already
    /// unlocked, or from the periodic sweep below for users who happen to
    /// have a session open at that moment) -- this never prompts for a
    /// PIN itself. If liquid balance in the active wallet is above
    /// `yield_reserve_lamports`, stakes the excess. Silently does nothing
    /// if the excess is dust-sized (not worth a swap's own network fee)
    /// or if `user.yield_auto_enabled` is off.
    async fn maybe_auto_sweep_idle(&self, chat_id: i64, user: &mut UserRecord, keypair: &Keypair) {
        if !user.yield_auto_enabled {
            return;
        }
        const MIN_SWEEP_LAMPORTS: u64 = 20_000_000; // 0.02 SOL -- below this, the swap's own fee isn't worth it
        let balance_lamports = match self.rpc.get_balance_lamports(&user.active().pubkey).await {
            Ok(b) => b,
            Err(_) => return, // best-effort -- a failed balance check here should never surface as an error to the user
        };
        if balance_lamports <= self.yield_reserve_lamports {
            return;
        }
        let excess_lamports = balance_lamports - self.yield_reserve_lamports;
        if excess_lamports < MIN_SWEEP_LAMPORTS {
            return;
        }
        let amount_sol = excess_lamports as f64 / LAMPORTS_PER_SOL;
        let _ = self.tg.send_html(chat_id, &format!("🌾 Auto-staking {amount_sol:.4} idle SOL into yield..."), None).await;
        let _ = self.execute_stake(chat_id, user, keypair, amount_sol).await;
    }

    /// Auto-Yield's periodic pass -- runs on a fixed interval (spawned
    /// from main.rs), checking every user with `yield_auto_enabled` on.
    /// IMPORTANT LIMITATION (by design, not a bug): this can only act on
    /// users who happen to have a live trading session unlocked at the
    /// moment this runs, since Wraith has no master key and never caches
    /// a PIN beyond the normal 15-minute session window -- there is no
    /// way for a background task to decrypt a wallet on its own. For a
    /// user who isn't actively using the bot, idle SOL gets swept the
    /// next time THEY unlock trading (buy/sell/stake), not while they're
    /// fully offline. This is the correct trade-off given the existing
    /// "no master key, ever" security model -- see crypto.rs.
    pub async fn run_yield_sweep(&self) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await; // every 5 minutes
            let mut candidates: Vec<UserRecord> = vec![];
            for item in self.db.inner_iter() {
                let Ok((key, bytes)) = item else { continue };
                if !key.starts_with(b"user:") {
                    continue;
                }
                if let Ok(user) = serde_json::from_slice::<UserRecord>(&bytes) {
                    if user.yield_auto_enabled {
                        candidates.push(user);
                    }
                }
            }
            for mut user in candidates {
                let telegram_id = user.telegram_id;
                let Some(kp) = self.session_keypair(telegram_id) else { continue }; // no live session -- skip, see doc comment above
                self.maybe_auto_sweep_idle(telegram_id, &mut user, &kp).await; // Telegram user id == their own DM chat id
            }
        }
    }

    async fn execute_buy(&self, chat_id: i64, user: &mut UserRecord, keypair: &Keypair, ca: &str, amount_sol: f64) -> Result<()> {
        // Auto-Yield's "unstake when needed" half: if the user's liquid
        // SOL can't cover this buy, unstake their full JitoSOL position
        // first (same gains-only fee as a manual unstake -- see
        // execute_unstake) so the buy can proceed. Any unstaked amount
        // beyond what this buy needs stays liquid; it'll get swept back
        // into yield next time maybe_auto_sweep_idle runs, if still on.
        if user.yield_auto_enabled {
            let needed_lamports = (amount_sol * LAMPORTS_PER_SOL) as u64;
            let current_lamports = self.rpc.get_balance_lamports(&user.active().pubkey).await.unwrap_or(0);
            if current_lamports < needed_lamports {
                self.tg.send_html(chat_id, "🌾 Unstaking yield to cover this buy...", None).await?;
                // Best-effort: if this fails (nothing staked, quote
                // error, etc), fall through and let the ordinary
                // insufficient-balance/quote-failure handling below
                // explain it -- no point stacking two error messages.
                let _ = self.execute_unstake(chat_id, user, keypair).await;
            }
        }

        self.tg.send_html(chat_id, "⚡ Getting quote...", None).await?;

        let lamports = (amount_sol * LAMPORTS_PER_SOL) as u64;
        let quote = match self.jup.get_quote(SOL_MINT, ca, lamports, user.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't get a swap quote: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        if let Some(impact) = price_impact_pct(&quote) {
            if impact > 3.0 {
                self.tg.send_html(chat_id, &format!(
                    "⚠️ <b>High Price Impact: {impact:.2}%</b>\n\nThis trade will move the price significantly due to low liquidity. You may receive much less than expected.\n\nExecuting anyway..."
                ), None).await?;
            }
        }

        self.log_platform_fee(&quote, "buy");

        self.tg.send_html(chat_id, "⚡ Executing swap...", None).await?;

        match self.sign_and_send_swap(keypair, &user.active().pubkey, &quote, &self.fee_wallet).await {
            Ok(sig) => {
                let est_out_raw = out_amount(&quote).unwrap_or(0);

                let decimals = self.rpc.get_mint_decimals(ca).await.unwrap_or(9);
                let pair = dexscreener::get_token_pair(ca).await.ok().flatten();
                let entry_price_usd = pair
                    .as_ref()
                    .and_then(|p| p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()))
                    .unwrap_or(0.0);
                // Real ticker from DexScreener when available (e.g. "BONK");
                // falls back to the old truncated-CA display only if the
                // token has no DexScreener listing yet (very fresh launches).
                let symbol = pair
                    .as_ref()
                    .and_then(|p| p["baseToken"]["symbol"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| crate::telegram::escape_html(s))
                    .unwrap_or_else(|| ca.chars().take(6).collect::<String>().to_uppercase());

                let human_tokens = est_out_raw as f64 / 10f64.powi(decimals as i32);
                let spent_usd_est = if entry_price_usd > 0.0 { human_tokens * entry_price_usd } else { 0.0 };
                let usd_line = if spent_usd_est > 0.0 { format!(" (~${spent_usd_est:.2})") } else { String::new() };
                let entry_line = if entry_price_usd > 0.0 { format!("\n💵 Entry price: ${entry_price_usd:.8}") } else { String::new() };

                user.active_mut().positions.push(Position {
                    mint: ca.to_string(),
                    symbol: symbol.clone(),
                    sol_spent: amount_sol,
                    tokens_received_est: human_tokens,
                    timestamp: crate::state::chrono_now(),
                    entry_price_usd,
                    decimals,
                });
                self.db.save_user(user)?;
                self.tg.send_html(chat_id, &format!(
                    "✅ <b>Swap sent</b>\n\n💸 Spent: {amount_sol} SOL{usd_line}\n🪙 Tokens: {human_tokens:.2}{entry_line}\n🔗 Tx: <code>{sig}</code>\n\nCheck the signature on Solscan to confirm it landed."
                ), Some(kb::main_only())).await?;
            }
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Swap failed: {e}"), Some(kb::main_only())).await?;
            }
        }
        Ok(())
    }

    async fn execute_sell(&self, chat_id: i64, user: &mut UserRecord, keypair: &Keypair, ca: &str, pct: u8) -> Result<()> {
        let (raw_balance, decimals) = match self.rpc.get_token_balance(&user.active().pubkey, ca).await {
            Ok(v) => v,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't check your token balance: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };
        if raw_balance == 0 {
            self.tg.send_html(chat_id, "❌ You don't hold any of this token.", Some(kb::main_only())).await?;
            return Ok(());
        }

        let pct = pct.clamp(1, 100);
        let sell_raw: u64 = if pct >= 100 {
            raw_balance
        } else {
            (((raw_balance as u128) * pct as u128) / 100) as u64
        };
        if sell_raw == 0 {
            self.tg.send_html(chat_id, "❌ That percentage rounds down to zero tokens — pick a higher percentage.", Some(kb::main_only())).await?;
            return Ok(());
        }

        self.tg.send_html(chat_id, &format!("⚡ Getting quote and executing {pct}% sell..."), None).await?;

        let quote = match self.jup.get_quote(ca, SOL_MINT, sell_raw, user.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't get a swap quote: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        self.log_platform_fee(&quote, "sell");

        match self.sign_and_send_swap(keypair, &user.active().pubkey, &quote, &self.fee_wallet).await {
            Ok(sig) => {
                let est_out_sol = out_amount(&quote).unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                let human_amount = sell_raw as f64 / 10f64.powi(decimals as i32);

                if pct >= 100 {
                    user.active_mut().positions.retain(|p| p.mint != ca);
                } else if let Some(p) = user.active_mut().positions.iter_mut().find(|p| p.mint == ca) {
                    // Reduce the tracked cost-basis/holdings by the sold
                    // fraction so PnL on the remainder stays meaningful,
                    // rather than deleting or leaving the position stale.
                    let frac_remaining = 1.0 - (pct as f64 / 100.0);
                    p.sol_spent *= frac_remaining;
                    p.tokens_received_est *= frac_remaining;
                }
                self.db.save_user(user)?;

                self.tg.send_html(chat_id, &format!(
                    "✅ <b>Sell sent</b>\n\n🪙 Sold: ~{human_amount:.4} tokens ({pct}%)\n💰 Est. received: {est_out_sol:.4} SOL\n🔗 Tx: <code>{sig}</code>"
                ), Some(kb::main_only())).await?;

                // Proceeds just landed as liquid SOL -- a natural moment
                // to sweep into yield if the user has Auto-Yield on.
                self.maybe_auto_sweep_idle(chat_id, user, keypair).await;
            }
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Swap failed: {e}"), Some(kb::main_only())).await?;
            }
        }
        Ok(())
    }

    /// Prints what Jupiter's quote actually says about the platform fee
    /// for this trade, so a silently-missing fee shows up in the server
    /// logs immediately instead of only being noticed (much later) as a
    /// missing deposit in the fee wallet. If FEE_WALLET is configured but
    /// `platformFee` is absent/zero on a quote, that's the single
    /// clearest signal something is wrong with fee collection --
    /// most commonly the fee wallet's wrapped-SOL token account not
    /// existing (or having been closed) on-chain. See the note on
    /// `jupiter::derive_wsol_fee_account` for why that account has to
    /// exist ahead of time.
    fn log_platform_fee(&self, quote: &serde_json::Value, side: &str) {
        if self.fee_wallet.is_empty() {
            return;
        }
        match quote.get("platformFee") {
            Some(pf) if pf.is_object() => {
                let amount = pf["amount"].as_str().unwrap_or("0");
                let fee_bps = pf["feeBps"].as_u64().unwrap_or(0);
                println!("💵 [{side}] platform fee in quote: amount={amount} feeBps={fee_bps}");
            }
            _ => {
                eprintln!(
                    "⚠️ [{side}] quote had NO platformFee field even though FEE_WALLET is set. \
                     This trade will NOT pay a platform fee. Most likely cause: the fee \
                     wallet's wrapped-SOL token account doesn't exist on-chain (or was closed \
                     after being unwrapped). Verify/recreate it -- see derive_wsol_fee_account \
                     in jupiter.rs."
                );
            }
        }
    }

    /// `fee_wallet`: pass `&self.fee_wallet` for user-initiated trades
    /// (buy/sell -- takes the 0.75% platform fee), or `""` for the bot
    /// moving a user's own money into/out of yield -- that's not a trade,
    /// so no trading fee applies there (the yield fee, taken separately
    /// in execute_unstake, is the only cut on that flow).
    async fn sign_and_send_swap(&self, keypair: &Keypair, pubkey: &str, quote: &serde_json::Value, fee_wallet: &str) -> Result<String> {
        let swap_tx_b64 = self.jup.get_swap_transaction(quote, pubkey, fee_wallet, self.max_priority_fee_lamports).await?;

        let tx_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, swap_tx_b64)?;
        let mut versioned_tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;

        let message_bytes = versioned_tx.message.serialize();
        let signature = keypair.sign_message(&message_bytes);
        if versioned_tx.signatures.is_empty() {
            versioned_tx.signatures.push(signature);
        } else {
            versioned_tx.signatures[0] = signature;
        }

        let signed_bytes = bincode::serialize(&versioned_tx)?;
        let signed_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, signed_bytes);

        self.rpc.send_raw_transaction_b64(&signed_b64).await
    }

    async fn do_withdraw(&self, chat_id: i64, keypair: &Keypair, dest: &str, amount_sol: f64) -> Result<()> {
        let dest_pubkey = match Pubkey::from_str(dest) {
            Ok(p) => p,
            Err(_) => {
                self.tg.send_html(chat_id, "❌ Invalid destination address.", Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        let lamports = (amount_sol * LAMPORTS_PER_SOL) as u64;
        let blockhash_str = self.rpc.get_latest_blockhash().await?;
        let blockhash = solana_sdk::hash::Hash::from_str(&blockhash_str)?;

        let instruction = system_instruction::transfer(&keypair.pubkey(), &dest_pubkey, lamports);
        let mut tx = Transaction::new_with_payer(&[instruction], Some(&keypair.pubkey()));
        tx.sign(&[keypair], blockhash);

        let tx_bytes = bincode::serialize(&tx)?;
        let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tx_bytes);

        match self.rpc.send_raw_transaction_b64(&tx_b64).await {
            Ok(sig) => {
                self.tg.send_html(chat_id, &format!("✅ <b>Withdrawal sent</b>\n\n💸 {amount_sol:.4} SOL → <code>{dest}</code>\n🔗 Tx: <code>{sig}</code>"), Some(kb::main_only())).await?;
            }
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Withdrawal failed: {e}"), Some(kb::main_only())).await?;
            }
        }
        Ok(())
    }

    async fn handle_rug_scan(&self, chat_id: i64, ca: &str) -> Result<()> {
        let pair = match dexscreener::get_token_pair(ca).await {
            Ok(Some(p)) => p,
            _ => {
                self.tg.send_html(chat_id, "❌ Token not found on DexScreener.", Some(kb::main_only())).await?;
                return Ok(());
            }
        };
        let a = dexscreener::analyze(&pair);
        let bar_filled = (a.score / 10).clamp(0, 10) as usize;
        let bar = "█".repeat(bar_filled) + &"░".repeat(10 - bar_filled);
        let flags = if a.flags.is_empty() { "✅ No major red flags detected\n".to_string() } else { format!("🚩 <b>Red Flags:</b>\n{}\n", a.flags.join("\n")) };
        let good = if a.good.is_empty() { String::new() } else { format!("💚 <b>Positives:</b>\n{}", a.good.join("\n")) };

        self.tg.send_html(chat_id, &format!(
            "🤖 <b>AI Rug Analysis</b>\n\n<b>Risk Score: {}/100</b>\n<code>[{bar}]</code>\n{} <b>{}</b>\n\n{flags}{good}",
            a.score, a.risk_emoji, a.risk_level
        ), Some(kb::main_only())).await?;
        Ok(())
    }

    /// Layers on-chain safety checks (mint/freeze authority, holder
    /// concentration) on top of the DexScreener-only market score.
    async fn apply_onchain_checks(&self, ca: &str, signal: &mut dexscreener::GemSignal) {
        if signal.score < 45 {
            return;
        }

        if let Ok((mint_ok, freeze_ok)) = self.rpc.get_mint_authority_status(ca).await {
            if mint_ok && freeze_ok {
                signal.score = (signal.score + 15).min(100);
                signal.notes.insert(0, "✅ Mint & freeze authority renounced".to_string());
            } else {
                signal.score = (signal.score - 25).max(0);
                signal.notes.insert(0, "🚨 Mint/freeze authority still active — creator retains rug control".to_string());
            }
        }

        if let Ok(Some(conc)) = self.rpc.get_top10_concentration_pct(ca).await {
            if conc > 70.0 {
                signal.score = (signal.score - 15).max(0);
                signal.notes.push(format!("🚨 Top 10 wallets hold ~{conc:.0}% of supply"));
            } else if conc < 30.0 {
                signal.notes.push(format!("✅ Reasonably distributed (top 10 ~{conc:.0}%)"));
            }
        }

        signal.tier = dexscreener::tier_for_score(signal.score);
    }

    /// Handles one event from the PumpPortal WebSocket feed -- the earliest
    /// possible signal, since tokens land here at creation, before
    /// DexScreener has any listing for them.
    /// No more per-event alerts here -- pump.fun throws off way too much
    /// volume for a message-per-token to be usable. We just keep the
    /// PumpWatch record up to date (curve progress, cached authority
    /// check, migration status); the AI Gem Scanner (`handle_gem_scan`)
    /// is what actually scores and surfaces the worthwhile ones, on
    /// demand, filtered down to Moderate/High potential only.
    pub async fn handle_pump_event(&self, event: PumpEvent) {
        match event {
            PumpEvent::NewToken(data) => {
                if self.db.get_pump_watch(&data.mint).ok().flatten().is_none() {
                    let _ = self.db.save_pump_watch(&data.mint, &crate::db::PumpWatch {
                        mint: data.mint.clone(),
                        name: data.name,
                        symbol: data.symbol,
                        first_seen: crate::state::chrono_now(),
                        ..Default::default()
                    });
                }
            }

            PumpEvent::CurveProgress(data) => {
                // Spawn so an on-chain lookup for one token never delays
                // processing of the next PumpPortal event in the stream.
                let app = self.clone();
                tokio::spawn(async move {
                    app.update_pump_watch_curve(data).await;
                });
            }

            PumpEvent::Migrated(data) => {
                let mut watch = self.db.get_pump_watch(&data.mint).ok().flatten().unwrap_or_else(|| crate::db::PumpWatch {
                    mint: data.mint.clone(),
                    name: data.name.clone(),
                    symbol: data.symbol.clone(),
                    first_seen: crate::state::chrono_now(),
                    ..Default::default()
                });
                watch.migrated = true;
                watch.migrated_at = crate::state::chrono_now();
                let _ = self.db.save_pump_watch(&data.mint, &watch);
            }
        }
    }

    /// Refreshes a pre-migration token's bonding-curve progress and cached
    /// "authorities renounced" check. Runs off the hot event-loop path
    /// (see `handle_pump_event`) since the RPC call can take a moment.
    async fn update_pump_watch_curve(&self, data: crate::pumpportal::TokenData) {
        let mut watch = self.db.get_pump_watch(&data.mint).ok().flatten().unwrap_or_else(|| crate::db::PumpWatch {
            mint: data.mint.clone(),
            name: data.name.clone(),
            symbol: data.symbol.clone(),
            first_seen: crate::state::chrono_now(),
            ..Default::default()
        });
        watch.last_curve_pct = (data.v_sol_in_bonding_curve / MIGRATION_SOL_APPROX_FOR_DISPLAY * 100.0).min(100.0);
        if let Ok((mint_ok, freeze_ok)) = self.rpc.get_mint_authority_status(&data.mint).await {
            watch.authorities_ok = Some(mint_ok && freeze_ok);
        }
        let _ = self.db.save_pump_watch(&data.mint, &watch);
    }

    /// Scores a still-pre-migration pump.fun token using only what we know
    /// before it has any DexScreener listing: renounced authorities,
    /// bonding-curve progress, and freshness. Same 0-100 scale / tier
    /// labels as the DexScreener-based scorer so both sources show up
    /// side-by-side in the AI Gem Scanner as one consistent list.
    fn score_pump_watch(watch: &crate::db::PumpWatch) -> (i32, Vec<String>) {
        let mut score = 0i32;
        let mut notes = vec![];

        match watch.authorities_ok {
            Some(true) => {
                score += 40;
                notes.push("✅ Mint & freeze authority renounced".to_string());
            }
            Some(false) => {
                return (0, vec!["🚨 Mint/freeze authority still active — skipped".to_string()]);
            }
            None => {
                // Haven't gotten a curve-progress event (and therefore an
                // authority check) for this one yet -- treat as unknown,
                // neither rewarded nor zeroed out.
            }
        }

        if watch.last_curve_pct >= 50.0 && watch.last_curve_pct < 95.0 {
            score += 30;
            notes.push(format!("📈 ~{:.0}% toward migration — real buying pressure", watch.last_curve_pct));
        } else if watch.last_curve_pct >= 20.0 {
            score += 15;
            notes.push(format!("📈 ~{:.0}% toward migration", watch.last_curve_pct));
        } else if watch.last_curve_pct > 0.0 {
            score += 5;
        }

        let age_hours = (crate::state::chrono_now() - watch.first_seen) as f64 / 3600.0;
        if age_hours < 1.0 {
            score += 10;
            notes.push("🆕 Brand new (under 1h)".to_string());
        } else if age_hours < 6.0 {
            score += 15;
            notes.push("🆕 Very early (under 6h)".to_string());
        } else {
            score += 5;
        }

        (score.clamp(0, 100), notes)
    }

    async fn broadcast_to_gem_alert_subscribers(&self, msg: &str, kb: Option<Vec<Vec<crate::telegram::InlineButton>>>) {
        for item in self.db.inner_iter() {
            if let Ok((_, bytes)) = item {
                if let Ok(user) = serde_json::from_slice::<crate::state::UserRecord>(&bytes) {
                    if user.gem_alerts {
                        self.tg.send_html(user.telegram_id, msg, kb.clone()).await.ok();
                    }
                }
            }
        }
    }

    pub async fn run_gem_scanner(&self) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            let addrs = match dexscreener::get_candidate_addresses().await {
                Ok(a) if !a.is_empty() => a,
                _ => continue,
            };
            let pairs = match dexscreener::get_pairs_for_addresses(&addrs).await {
                Ok(p) => p,
                Err(_) => continue,
            };

            for pair in &pairs {
                let ca = match pair["baseToken"]["address"].as_str() {
                    Some(a) => a.to_string(),
                    None => continue,
                };

                let prev = self.db.get_gem_snapshot(&ca).ok().flatten();
                let prev_snap = prev.as_ref().map(|p| dexscreener::Snapshot { liq_usd: p.liq_usd, vol_h24: p.vol_h24 });
                let mut signal = dexscreener::score_gem(pair, prev_snap.as_ref());
                self.apply_onchain_checks(&ca, &mut signal).await;

                let liq = pair["liquidity"]["usd"].as_f64().unwrap_or(0.0);
                let vol = pair["volume"]["h24"].as_f64().unwrap_or(0.0);
                let now = crate::state::chrono_now();
                let already_alerted = prev.as_ref().map(|p| p.alerted).unwrap_or(false);
                let first_seen = prev.as_ref().map(|p| p.first_seen).unwrap_or(now);

                // Only push alerts for the top tier (score >= 75, "🚀 HIGH
                // POTENTIAL" -- see dexscreener::tier_for_score). Previously
                // fired at >= 60, which also included "⚡ MODERATE
                // POTENTIAL" calls -- too much noise for an unattended
                // push notification. Users can still see moderate-tier
                // tokens on demand via "📊 Live Signals" / "💎 AI Gem
                // Scanner" in the menu; this only changes what gets
                // proactively pushed to them.
                let should_alert = !already_alerted && signal.score >= 75;
                let _ = self.db.save_gem_snapshot(&ca, &crate::db::GemSnapshot {
                    liq_usd: liq,
                    vol_h24: vol,
                    first_seen,
                    alerted: already_alerted || should_alert,
                });

                if !should_alert { continue; }

                let symbol = pair["baseToken"]["symbol"].as_str().unwrap_or("???");
                let name = pair["baseToken"]["name"].as_str().unwrap_or("Unknown");
                let name_esc = crate::telegram::escape_html(name);
                let symbol_esc = crate::telegram::escape_html(symbol);
                let mc = pair["fdv"].as_f64().map(fmt_usd).unwrap_or_else(|| "N/A".to_string());
                let liq_disp = fmt_usd(liq);
                let change1h = pair["priceChange"]["h1"].as_f64().unwrap_or(0.0);
                let fresh_tag = if signal.is_fresh { "🆕 " } else { "" };
                let notes = signal.notes.iter().take(4).map(|n| format!("• {}", crate::telegram::escape_html(n))).collect::<Vec<_>>().join("\n");

                let msg = format!(
                    "💎 <b>{fresh_tag}Gem Alert!</b>\n\n🪙 <b>{name_esc} (${symbol_esc})</b>\n📋 <code>{ca}</code>\n💎 MC: {mc} | 💧 Liq: {liq_disp} | 📈 {change1h:+.1}% (1h)\n🤖 Score: {}/100 — {}\n{notes}\n\n<i>Tap buy to trade instantly 👇</i>",
                    signal.score, signal.tier
                );
                let kb = vec![
                    vec![crate::telegram::btn(&format!("🚀 Buy ${symbol}"), &format!("bcp_{ca}"))],
                    vec![crate::telegram::btn("❌ Skip", "main")],
                ];

                self.broadcast_to_gem_alert_subscribers(&msg, Some(kb)).await;
            }
        }
    }

    async fn handle_pnl(&self, chat_id: i64, telegram_id: i64) -> Result<()> {
        let user = self.get_or_create_user(telegram_id)?;
        if user.active().positions.is_empty() {
            self.tg.send_html(chat_id, "📈 <b>PnL Summary</b>\n\nNo positions tracked yet. Buy a token first!", Some(kb::main_only())).await?;
            return Ok(());
        }

        self.tg.send_html(chat_id, "📈 Calculating PnL...", None).await?;

        let sol_price_usd = dexscreener::get_token_pair(SOL_MINT)
            .await.ok().flatten()
            .and_then(|p| p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()))
            .unwrap_or(0.0);

        let mut set = tokio::task::JoinSet::new();
        for (i, p) in user.active().positions.iter().enumerate() {
            let mint = p.mint.clone();
            set.spawn(async move {
                let price = dexscreener::get_token_pair(&mint)
                    .await.ok().flatten()
                    .and_then(|pair| pair["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()));
                (i, price)
            });
        }
        let mut prices: Vec<Option<f64>> = vec![None; user.active().positions.len()];
        while let Some(res) = set.join_next().await {
            if let Ok((i, price)) = res { prices[i] = price; }
        }

        let mut total_invested_sol = 0.0f64;
        let mut total_current_usd = 0.0f64;
        let mut total_invested_usd = 0.0f64;
        let mut msg = "📈 <b>PnL Summary</b>\n\n".to_string();

        for (i, p) in user.active().positions.iter().enumerate() {
            total_invested_sol += p.sol_spent;
            match prices[i] {
                Some(cur) if p.entry_price_usd > 0.0 => {
                    let pct = (cur / p.entry_price_usd - 1.0) * 100.0;
                    let entry_val = p.tokens_received_est * p.entry_price_usd;
                    let cur_val = p.tokens_received_est * cur;
                    let pl_usd = cur_val - entry_val;
                    let pl_sol = if sol_price_usd > 0.0 { pl_usd / sol_price_usd } else { 0.0 };
                    total_invested_usd += entry_val;
                    total_current_usd += cur_val;
                    let arrow = if pct >= 0.0 { "🟢" } else { "🔴" };
                    msg += &format!("{arrow} <b>{}</b> {pct:+.1}% | P/L: {pl_sol:+.4} SOL (${pl_usd:+.2})\n", p.symbol);
                }
                _ => {
                    msg += &format!("⚪ <b>{}</b> — price unavailable\n", p.symbol);
                }
            }
        }

        let total_pl_usd = total_current_usd - total_invested_usd;
        let total_pl_sol = if sol_price_usd > 0.0 { total_pl_usd / sol_price_usd } else { 0.0 };
        let total_pct = if total_invested_usd > 0.0 { (total_current_usd / total_invested_usd - 1.0) * 100.0 } else { 0.0 };

        msg += &format!(
            "\n━━━━━━━━━━━━━━\n💼 <b>Total invested:</b> {total_invested_sol:.4} SOL\n📊 <b>Overall P/L:</b> {total_pl_sol:+.4} SOL (${total_pl_usd:+.2}) {total_pct:+.1}%"
        );

        self.tg.send_html(chat_id, &msg, Some(kb::main_only())).await?;
        Ok(())
    }

    /// Only Moderate potential (⚡ 55+) and above get shown here -- this is
    /// the "worthy ones only" bar for the whole scanner, covering both the
    /// regular DexScreener sweep and the pump.fun (pre/post-migration)
    /// candidates below.
    const GEM_SCAN_MIN_SCORE: i32 = 55;

    async fn handle_gem_scan(&self, chat_id: i64) -> Result<()> {
        self.tg.send_html(chat_id, "💎 <b>AI Gem Scanner</b>\n\nScanning Solana (incl. pump.fun pre/post-migration) for high-potential tokens...", None).await?;

        let mut entries: Vec<GemEntry> = vec![];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1) Regular DexScreener sweep (established + newly-listed tokens).
        let pairs = dexscreener::get_trending_solana_pairs().await.unwrap_or_default();
        for pair in &pairs {
            let ca = pair["baseToken"]["address"].as_str().unwrap_or("").to_string();
            if ca.is_empty() || seen.contains(&ca) {
                continue;
            }
            let prev = self.db.get_gem_snapshot(&ca).ok().flatten();
            let prev_snap = prev.as_ref().map(|p| dexscreener::Snapshot { liq_usd: p.liq_usd, vol_h24: p.vol_h24 });
            let mut signal = dexscreener::score_gem(pair, prev_snap.as_ref());
            self.apply_onchain_checks(&ca, &mut signal).await;
            if signal.score < Self::GEM_SCAN_MIN_SCORE {
                continue;
            }
            seen.insert(ca.clone());
            entries.push(GemEntry {
                ca,
                name: pair["baseToken"]["name"].as_str().unwrap_or("Unknown").to_string(),
                symbol: pair["baseToken"]["symbol"].as_str().unwrap_or("???").to_string(),
                score: signal.score,
                tier: signal.tier,
                notes: signal.notes,
                mc: pair["fdv"].as_f64(),
                liq: pair["liquidity"]["usd"].as_f64(),
                change1h: pair["priceChange"]["h1"].as_f64(),
                is_fresh: signal.is_fresh,
                pre_migration: false,
            });
        }

        // 2) Pump.fun candidates we've been quietly tracking -- both
        // still-on-the-curve tokens and ones that just migrated but may
        // not have hit the DexScreener trending feed yet.
        for item in self.db.inner_iter() {
            let Ok((key, bytes)) = item else { continue };
            if !key.starts_with(b"pump:") {
                continue;
            }
            let Ok(watch) = serde_json::from_slice::<crate::db::PumpWatch>(&bytes) else { continue };
            if watch.mint.is_empty() || seen.contains(&watch.mint) {
                continue;
            }
            let age_hours = (crate::state::chrono_now() - watch.first_seen) as f64 / 3600.0;
            if age_hours > 12.0 {
                continue; // stale -- bonding-curve tokens rarely stay relevant this long
            }

            if watch.migrated {
                let Ok(Some(pair)) = dexscreener::get_token_pair(&watch.mint).await else { continue };
                let mut signal = dexscreener::score_gem(&pair, None);
                self.apply_onchain_checks(&watch.mint, &mut signal).await;
                if signal.score < Self::GEM_SCAN_MIN_SCORE {
                    continue;
                }
                seen.insert(watch.mint.clone());
                entries.push(GemEntry {
                    ca: watch.mint.clone(),
                    name: pair["baseToken"]["name"].as_str().unwrap_or("Unknown").to_string(),
                    symbol: pair["baseToken"]["symbol"].as_str().unwrap_or("???").to_string(),
                    score: signal.score,
                    tier: signal.tier,
                    notes: signal.notes,
                    mc: pair["fdv"].as_f64(),
                    liq: pair["liquidity"]["usd"].as_f64(),
                    change1h: pair["priceChange"]["h1"].as_f64(),
                    is_fresh: signal.is_fresh,
                    pre_migration: false,
                });
            } else {
                let (score, notes) = Self::score_pump_watch(&watch);
                if score < Self::GEM_SCAN_MIN_SCORE {
                    continue;
                }
                seen.insert(watch.mint.clone());
                entries.push(GemEntry {
                    ca: watch.mint.clone(),
                    name: if watch.name.is_empty() { "Unknown".to_string() } else { watch.name.clone() },
                    symbol: if watch.symbol.is_empty() { "???".to_string() } else { watch.symbol.clone() },
                    score,
                    tier: dexscreener::tier_for_score(score),
                    notes,
                    mc: None,
                    liq: None,
                    change1h: None,
                    is_fresh: age_hours < 6.0,
                    pre_migration: true,
                });
            }
        }

        if entries.is_empty() {
            self.tg.send_html(chat_id, "💎 <b>AI Gem Scanner</b>\n\nNo standout gems found right now (nothing cleared the Moderate+ bar). Try again in a few minutes for fresh signals.", Some(kb::main_only())).await?;
            return Ok(());
        }

        entries.sort_by(|a, b| b.score.cmp(&a.score));
        entries.truncate(5);

        let mut msg = "💎 <b>AI Gem Scanner — Top Picks</b>\n\n".to_string();
        for e in &entries {
            let name_esc = crate::telegram::escape_html(&e.name);
            let symbol_esc = crate::telegram::escape_html(&e.symbol);
            let fresh_tag = if e.is_fresh { "🆕 " } else { "" };
            let top_note = if e.notes.is_empty() { String::new() } else {
                format!("\n{}", e.notes.iter().take(2).map(|n| crate::telegram::escape_html(n)).collect::<Vec<_>>().join(" | "))
            };

            if e.pre_migration {
                msg += &format!(
                    "━━━━━━━━━━━━\n🪙 <b>{fresh_tag}{name_esc} (${symbol_esc})</b>\n📋 <code>{}</code>\n⏳ Pre-migration (bonding curve) — not tradeable through this bot yet\n🤖 Score: {}/100 — {}{top_note}\n\n",
                    e.ca, e.score, e.tier
                );
            } else {
                let mc = e.mc.map(fmt_usd).unwrap_or_else(|| "N/A".to_string());
                let liq = e.liq.map(fmt_usd).unwrap_or_else(|| "N/A".to_string());
                let change1h = e.change1h.unwrap_or(0.0);
                msg += &format!(
                    "━━━━━━━━━━━━\n🪙 <b>{fresh_tag}{name_esc} (${symbol_esc})</b>\n📋 <code>{}</code>\n💎 MC: {mc} | 💧 Liq: {liq} | 📈 {change1h:+.1}% (1h)\n🤖 Score: {}/100 — {}{top_note}\n\n",
                    e.ca, e.score, e.tier
                );
            }
        }
        msg += "⚠️ <i>Not financial advice. Always DYOR before buying.</i>";

        let mut kb: Vec<Vec<crate::telegram::InlineButton>> = vec![];
        for e in &entries {
            if e.pre_migration {
                continue; // not tradeable through Jupiter until it migrates
            }
            let stars = if e.score >= 75 { "🚀" } else { "⚡" };
            kb.push(vec![
                crate::telegram::btn(&format!("{stars} Buy ${}", e.symbol), &format!("bcp_{}", e.ca)),
            ]);
        }
        kb.push(vec![crate::telegram::btn("🏠 Main Menu", "main")]);

        self.tg.send_html(chat_id, &msg, Some(kb)).await?;
        Ok(())
    }

    async fn handle_trade_signals(&self, chat_id: i64) -> Result<()> {
        let pairs = dexscreener::get_trending_solana_pairs().await.unwrap_or_default();
        if pairs.is_empty() {
            self.tg.send_html(chat_id, "❌ Couldn't fetch signals. Try again.", Some(kb::main_only())).await?;
            return Ok(());
        }
        let mut msg = "📊 <b>Live Solana Signals</b>\n\n".to_string();
        for p in &pairs {
            let symbol = crate::telegram::escape_html(p["baseToken"]["symbol"].as_str().unwrap_or("???"));
            let change1h = p["priceChange"]["h1"].as_f64().unwrap_or(0.0);
            let mc = p["fdv"].as_f64().map(fmt_usd).unwrap_or_else(|| "N/A".to_string());
            msg += &format!("• <b>{symbol}</b> {change1h:.1}% (1h) | MC: {mc}\n");
        }
        self.tg.send_html(chat_id, &msg, Some(kb::main_only())).await?;
        Ok(())
    }
}

fn lockout_message(user: &UserRecord) -> Option<String> {
    let now = crate::state::chrono_now();
    let secs = user.pin_lockout.seconds_remaining(now);
    if secs <= 0 {
        return None;
    }
    Some(format!("🔒 Too many wrong PIN attempts. Try again in {}.", fmt_duration(secs)))
}

fn fmt_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", (secs + 59) / 60)
    } else {
        format!("{}h", (secs + 3599) / 3600)
    }
}

/// Formats a USD amount with full comma thousands separators, e.g.
/// 1234567.0 -> "$1,234,567" (instead of the old abbreviated "$1.2M").
fn fmt_usd(v: f64) -> String {
    let neg = v < 0.0;
    let whole = v.abs().round() as i64;
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let grouped: String = grouped.chars().rev().collect();
    format!("${}{}", if neg { "-" } else { "" }, grouped)
}

fn short_wallet(w: &str) -> String {
    if w.len() < 8 {
        return w.to_string();
    }
    format!("{}...{}", &w[..4], &w[w.len() - 4..])
}
