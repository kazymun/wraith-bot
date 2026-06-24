use crate::crypto::{hash_pin, Crypto};
use crate::db::Db;
use crate::dexscreener;
use crate::jupiter::{out_amount, Jupiter, SOL_MINT};
use crate::keyboards as kb;
use crate::rpc::SolanaRpc;
use crate::state::{Awaiting, Position, UserRecord};
use crate::telegram::{TgClient, TgMessage};
use crate::wallet;
use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::system_instruction;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use std::str::FromStr;

pub struct App {
    pub tg: TgClient,
    pub db: Db,
    pub crypto: Crypto,
    pub rpc: SolanaRpc,
    pub jup: Jupiter,
    pub default_slippage_bps: u32,
    pub fee_wallet: String,
}

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

impl App {
    pub fn get_or_create_user(&self, telegram_id: i64) -> Result<UserRecord> {
        if let Some(u) = self.db.get_user(telegram_id)? {
            return Ok(u);
        }
        let (pubkey, nonce, cipher) = wallet::generate_encrypted_wallet(&self.crypto)?;
        let user = UserRecord::new(telegram_id, pubkey, nonce, cipher, self.default_slippage_bps);
        self.db.save_user(&user)?;
        Ok(user)
    }

    async fn main_menu_text(&self, chat_id: i64, user: &UserRecord) -> String {
        let balance_sol = self
            .rpc
            .get_balance_lamports(&user.pubkey)
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
            short_wallet(&user.pubkey)
        )
    }

    pub async fn show_main(&self, chat_id: i64, telegram_id: i64) -> Result<()> {
        let user = self.get_or_create_user(telegram_id)?;
        let text = self.main_menu_text(chat_id, &user).await;
        self.tg.send_html(chat_id, &text, Some(kb::main_menu())).await?;
        Ok(())
    }

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

        match user.awaiting.clone() {
            Awaiting::SettingPin => {
                if text.len() == 4 && text.chars().all(|c| c.is_ascii_digit()) {
                    user.pin_hash = Some(hash_pin(&text));
                    user.awaiting = Awaiting::None;
                    self.db.save_user(&user)?;
                    self.tg
                        .send_html(chat_id, "✅ <b>PIN set!</b> This will be required to export your key or withdraw funds.", None)
                        .await?;
                    self.show_main(chat_id, telegram_id).await?;
                } else {
                    self.tg.send_html(chat_id, "❌ PIN must be exactly 4 digits. Try again:", None).await?;
                }
            }

            Awaiting::VerifyingPinForExport => {
                if check_pin(&user, &text) {
                    user.awaiting = Awaiting::None;
                    self.db.save_user(&user)?;
                    match wallet::export_private_key_b58(&self.crypto, &user) {
                        Ok(key) => {
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
                        Err(e) => {
                            self.tg.send_html(chat_id, &format!("❌ Couldn't decrypt your key: {e}"), Some(kb::main_only())).await?;
                        }
                    }
                } else {
                    self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                }
            }

            Awaiting::VerifyingPinForWithdraw { dest, amount_sol } => {
                if check_pin(&user, &text) {
                    user.awaiting = Awaiting::None;
                    self.db.save_user(&user)?;
                    self.do_withdraw(chat_id, &user, &dest, amount_sol).await?;
                } else {
                    self.tg.send_html(chat_id, "❌ Wrong PIN. Try again, or /cancel.", None).await?;
                }
            }

            Awaiting::EnteringImportKey => {
                user.awaiting = Awaiting::None;
                match wallet::import_encrypted_wallet(&self.crypto, &text) {
                    Ok((pubkey, nonce, cipher)) => {
                        user.pubkey = pubkey.clone();
                        user.enc_nonce = nonce;
                        user.enc_cipher = cipher;
                        self.db.save_user(&user)?;
                        self.tg.send_html(chat_id,
                            &format!("✅ <b>Wallet Imported!</b>\n\n📍 <b>Address:</b> <code>{pubkey}</code>"),
                            Some(kb::wallet_menu()),
                        ).await?;
                    }
                    Err(e) => {
                        self.db.save_user(&user)?;
                        self.tg.send_html(chat_id, &format!("❌ {e}\n\nMake sure you're pasting a raw Solana private key (base58), not a seed phrase."), Some(kb::main_only())).await?;
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
                let balance_sol = self.rpc.get_balance_lamports(&user.pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                if balance_sol <= 0.0005 {
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "❌ Insufficient balance to withdraw.", Some(kb::main_only())).await?;
                    return Ok(());
                }
                let amount_sol = (balance_sol - 0.0005).max(0.0); // leave a little for fees/rent
                if user.pin_hash.is_some() {
                    user.awaiting = Awaiting::VerifyingPinForWithdraw { dest, amount_sol };
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, &format!("⬆️ Withdrawing ~{amount_sol:.4} SOL.\n\nEnter your PIN to confirm:"), None).await?;
                } else {
                    self.db.save_user(&user)?;
                    self.do_withdraw(chat_id, &user, &dest, amount_sol).await?;
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
                self.handle_sell_ca(chat_id, &user, &text).await?;
            }

            Awaiting::EnteringRugScanCA => {
                user.awaiting = Awaiting::None;
                self.db.save_user(&user)?;
                self.handle_rug_scan(chat_id, &text).await?;
            }

            _ => {}
        }

        Ok(())
    }

    async fn handle_command(&self, chat_id: i64, telegram_id: i64, text: &str) -> Result<()> {
        let cmd = text.split_whitespace().next().unwrap_or("");
        match cmd {
            "/start" => {
                let user = self.get_or_create_user(telegram_id)?;

                if user.pin_hash.is_none() {
                    let mut u = user;
                    u.awaiting = Awaiting::SettingPin;
                    self.db.save_user(&u)?;
                    self.tg.send_html(chat_id,
                        "👻 <b>Welcome to Wraith</b>\n\nA real Solana wallet has been created for you.\n\n⚠️ <b>Important — read this once:</b>\nThis is a <b>custodial</b> wallet. Your private key is encrypted and stored on our server. The bot operator controls the master key. Only deposit funds you're comfortable with this arrangement.\n\nYou can export your private key anytime via Wallet → Export Private Key and move to your own wallet.\n\nNow set a 4-digit PIN to protect withdrawals and key export:\n\nReply with your PIN (e.g. 1234):",
                        None,
                    ).await?;
                } else {
                    self.show_main(chat_id, telegram_id).await?;
                }
            }
            "/cancel" => {
                let mut u = self.get_or_create_user(telegram_id)?;
                u.awaiting = Awaiting::None;
                self.db.save_user(&u)?;
                self.tg.send_html(chat_id, "Cancelled.", Some(kb::main_only())).await?;
            }
            "/help" => {
                self.tg.send_html(chat_id,
                    "👻 <b>Wraith Commands</b>\n\n/start — Main menu\n/buy — Buy a token\n/sell — Sell a token\n/balance — Check balance\n/cancel — Cancel current action\n/help — This message",
                    None,
                ).await?;
            }
            "/buy" => {
                let mut u = self.get_or_create_user(telegram_id)?;
                u.awaiting = Awaiting::EnteringBuyCA;
                self.db.save_user(&u)?;
                self.tg.send_html(chat_id, "🟢 <b>Buy Token</b>\n\nPaste the contract address (CA):", Some(kb::cancel_to("main"))).await?;
            }
            "/sell" => {
                let mut u = self.get_or_create_user(telegram_id)?;
                u.awaiting = Awaiting::EnteringSellCA;
                self.db.save_user(&u)?;
                self.tg.send_html(chat_id, "🔴 <b>Sell Token</b>\n\nPaste the CA to sell:", Some(kb::cancel_to("main"))).await?;
            }
            "/balance" => {
                let user = self.get_or_create_user(telegram_id)?;
                let balance_sol = self.rpc.get_balance_lamports(&user.pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                self.tg.send_html(chat_id, &format!("💰 <b>Balance:</b> {balance_sol:.4} SOL\n👛 <code>{}</code>", user.pubkey), None).await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn handle_callback(&self, callback_id: &str, data: &str, chat_id: i64, telegram_id: i64, message_id: Option<i64>) -> Result<()> {
        self.tg.answer_callback(callback_id, None).await.ok();
        let mut user = self.get_or_create_user(telegram_id)?;

        match data {
            "del_msg" => {
                if let Some(mid) = message_id {
                    self.tg.delete_message(chat_id, mid).await.ok();
                }
            }
            "main" | "refresh" => {
                self.show_main(chat_id, telegram_id).await?;
            }
            "wallet" => {
                let balance_sol = self.rpc.get_balance_lamports(&user.pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                self.tg.send_html(chat_id,
                    &format!("💰 <b>Wallet</b>\n\n📍 <b>Address:</b>\n<code>{}</code>\n\n💎 <b>Balance:</b> {balance_sol:.4} SOL\n\nSend SOL to this address to deposit.", user.pubkey),
                    Some(kb::wallet_menu()),
                ).await?;
            }
            "buy" => {
                user.awaiting = Awaiting::EnteringBuyCA;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "🟢 <b>Buy Token</b>\n\nPaste the contract address (CA):", Some(kb::cancel_to("main"))).await?;
            }
            "sell" => {
                user.awaiting = Awaiting::EnteringSellCA;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "🔴 <b>Sell Token</b>\n\nPaste the CA to sell:", Some(kb::cancel_to("main"))).await?;
            }
            "positions" => {
                if user.positions.is_empty() {
                    self.tg.send_html(chat_id, "📊 <b>Open Positions</b>\n\nNo open positions yet.", Some(kb::main_only())).await?;
                } else {
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
                    for (i, p) in user.positions.iter().enumerate() {
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
                    let mut prices: Vec<Option<f64>> = vec![None; user.positions.len()];
                    while let Some(res) = set.join_next().await {
                        if let Ok((i, price)) = res {
                            prices[i] = price;
                        }
                    }
                    let sol_price_usd = sol_handle.await.unwrap_or(0.0);

                    let mut msg = "📊 <b>Open Positions</b>\n\n".to_string();
                    for (i, p) in user.positions.iter().enumerate() {
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
                }
            }
            "withdraw" => {
                let balance_sol = self.rpc.get_balance_lamports(&user.pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                if balance_sol <= 0.0005 {
                    self.tg.send_html(chat_id, "❌ Insufficient balance to withdraw.", Some(kb::main_only())).await?;
                } else {
                    user.awaiting = Awaiting::EnteringWithdrawAddress;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "⬆️ <b>Withdraw</b>\n\nSend the destination wallet address:", Some(kb::cancel_to("wallet"))).await?;
                }
            }
            "export_key" => {
                if user.pin_hash.is_some() {
                    user.awaiting = Awaiting::VerifyingPinForExport;
                    self.db.save_user(&user)?;
                    self.tg.send_html(chat_id, "🔑 Enter your PIN to export your private key:", None).await?;
                } else {
                    self.tg.send_html(chat_id,
                        "🔒 You need to set a PIN before exporting your key. Go to ⚙️ Settings → Change PIN, then try again.",
                        Some(kb::main_only()),
                    ).await?;
                }
            }
            "import_wallet" => {
                user.awaiting = Awaiting::EnteringImportKey;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id,
                    "📥 <b>Import Wallet</b>\n\n⚠️ Only do this on a trusted device. Your current Wraith-generated wallet (and any funds in it) will no longer be accessible through this bot unless you save its key first.\n\nSend your Solana private key (base58):",
                    Some(kb::cancel_to("wallet")),
                ).await?;
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
            "settings" => {
                self.tg.send_html(chat_id, "⚙️ <b>Settings</b>", Some(kb::settings_menu())).await?;
            }
            "change_pin" => {
                user.awaiting = Awaiting::SettingPin;
                self.db.save_user(&user)?;
                self.tg.send_html(chat_id, "🔑 Enter your new 4-digit PIN:", None).await?;
            }
            "slippage" => {
                self.tg.send_html(chat_id, &format!("📊 Current slippage: {:.1}%\n\nSelect:", user.slippage_bps as f64 / 100.0), Some(kb::slippage_menu())).await?;
            }
            "referral" => {
                self.tg.send_html(chat_id, "👥 <b>Referral Program</b>\n\nComing soon.", Some(kb::main_only())).await?;
            }
            other if other.starts_with("buyamt_") => {
                self.handle_buy_amount(chat_id, telegram_id, other).await?;
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

    async fn handle_buy_ca(&self, chat_id: i64, ca: &str) -> Result<()> {
        self.tg.send_html(chat_id, "🔍 Scanning token...", None).await?;
        let pair = match dexscreener::get_token_pair(ca).await {
            Ok(Some(p)) => p,
            _ => {
                self.tg.send_html(chat_id, "❌ Token not found. Check the CA and try again.", Some(kb::cancel_to("main"))).await?;
                return Ok(());
            }
        };
        let a = dexscreener::analyze(&pair);
        let name = pair["baseToken"]["name"].as_str().unwrap_or("Unknown");
        let symbol = pair["baseToken"]["symbol"].as_str().unwrap_or("???");
        let mc = pair["fdv"].as_f64().map(|v| format!("${v:.0}")).unwrap_or_else(|| "N/A".to_string());
        let liq = pair["liquidity"]["usd"].as_f64().map(|v| format!("${v:.0}")).unwrap_or_else(|| "N/A".to_string());
        let price = pair["priceUsd"].as_str().unwrap_or("N/A");

        let flags = if a.flags.is_empty() { String::new() } else { format!("\n🚩 <b>Red Flags:</b>\n{}", a.flags.join("\n")) };
        let good = if a.good.is_empty() { String::new() } else { format!("\n💚 <b>Positives:</b>\n{}", a.good.join("\n")) };

        self.tg.send_html(chat_id, &format!(
            "🔍 <b>{name} ({symbol})</b>\n📋 <code>{ca}</code>\n\n💎 MC: {mc}\n💧 Liq: {liq}\n💵 Price: ${price}\n\n{} <b>Risk: {}/100 — {}</b>{flags}{good}\n\nSelect buy amount:",
            a.risk_emoji, a.score, a.risk_level
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
        let amount_sol: f64 = amt_str.parse().unwrap_or(0.0);
        let mut user = self.get_or_create_user(telegram_id)?;

        let balance_sol = self.rpc.get_balance_lamports(&user.pubkey).await.unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
        if balance_sol < amount_sol {
            self.tg.send_html(chat_id, &format!("❌ Insufficient balance. You have {balance_sol:.4} SOL, need {amount_sol} SOL."), Some(kb::main_only())).await?;
            return Ok(());
        }

        self.tg.send_html(chat_id, "⚡ Getting quote and executing...", None).await?;

        let lamports = (amount_sol * LAMPORTS_PER_SOL) as u64;
        let quote = match self.jup.get_quote(SOL_MINT, ca, lamports, user.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't get a swap quote: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        match self.sign_and_send_swap(&user, &quote).await {
            Ok(sig) => {
                let est_out_raw = out_amount(&quote).unwrap_or(0);
                let symbol = ca.chars().take(6).collect::<String>().to_uppercase();

                // Best-effort lookups — if either fails we still record the
                // position, just without a usable entry price for P&L later.
                let decimals = self.rpc.get_mint_decimals(ca).await.unwrap_or(9);
                let entry_price_usd = dexscreener::get_token_pair(ca)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|p| p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()))
                    .unwrap_or(0.0);

                let human_tokens = est_out_raw as f64 / 10f64.powi(decimals as i32);
                let spent_usd_est = if entry_price_usd > 0.0 { human_tokens * entry_price_usd } else { 0.0 };
                let usd_line = if spent_usd_est > 0.0 { format!(" (~${spent_usd_est:.2})") } else { String::new() };
                let entry_line = if entry_price_usd > 0.0 { format!("\n💵 Entry price: ${entry_price_usd:.8}") } else { String::new() };

                user.positions.push(Position {
                    mint: ca.to_string(),
                    symbol: symbol.clone(),
                    sol_spent: amount_sol,
                    tokens_received_est: human_tokens,
                    timestamp: crate::state::chrono_now(),
                    entry_price_usd,
                    decimals,
                });
                self.db.save_user(&user)?;
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

    async fn handle_sell_ca(&self, chat_id: i64, user: &UserRecord, ca: &str) -> Result<()> {
        let (raw_balance, decimals) = match self.rpc.get_token_balance(&user.pubkey, ca).await {
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

        self.tg.send_html(chat_id, "⚡ Getting quote and executing full sell...", None).await?;

        let quote = match self.jup.get_quote(ca, SOL_MINT, raw_balance, user.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Couldn't get a swap quote: {e}"), Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        match self.sign_and_send_swap(user, &quote).await {
            Ok(sig) => {
                let est_out_sol = out_amount(&quote).unwrap_or(0) as f64 / LAMPORTS_PER_SOL;
                let human_amount = raw_balance as f64 / 10f64.powi(decimals as i32);
                self.tg.send_html(chat_id, &format!(
                    "✅ <b>Sell sent</b>\n\n🪙 Sold: ~{human_amount:.4} tokens\n💰 Est. received: {est_out_sol:.4} SOL\n🔗 Tx: <code>{sig}</code>"
                ), Some(kb::main_only())).await?;
            }
            Err(e) => {
                self.tg.send_html(chat_id, &format!("❌ Swap failed: {e}"), Some(kb::main_only())).await?;
            }
        }
        Ok(())
    }

    async fn sign_and_send_swap(&self, user: &UserRecord, quote: &serde_json::Value) -> Result<String> {
        let keypair = wallet::load_keypair(&self.crypto, user)?;
        let swap_tx_b64 = self.jup.get_swap_transaction(quote, &user.pubkey, &self.fee_wallet).await?;

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

    async fn do_withdraw(&self, chat_id: i64, user: &UserRecord, dest: &str, amount_sol: f64) -> Result<()> {
        let dest_pubkey = match Pubkey::from_str(dest) {
            Ok(p) => p,
            Err(_) => {
                self.tg.send_html(chat_id, "❌ Invalid destination address.", Some(kb::main_only())).await?;
                return Ok(());
            }
        };

        let keypair = wallet::load_keypair(&self.crypto, user)?;
        let lamports = (amount_sol * LAMPORTS_PER_SOL) as u64;
        let blockhash_str = self.rpc.get_latest_blockhash().await?;
        let blockhash = solana_sdk::hash::Hash::from_str(&blockhash_str)?;

        let instruction = system_instruction::transfer(&keypair.pubkey(), &dest_pubkey, lamports);
        let mut tx = Transaction::new_with_payer(&[instruction], Some(&keypair.pubkey()));
        tx.sign(&[&keypair], blockhash);

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

    async fn handle_trade_signals(&self, chat_id: i64) -> Result<()> {
        let pairs = dexscreener::get_trending_solana_pairs().await.unwrap_or_default();
        if pairs.is_empty() {
            self.tg.send_html(chat_id, "❌ Couldn't fetch signals. Try again.", Some(kb::main_only())).await?;
            return Ok(());
        }
        let mut msg = "📊 <b>Live Solana Signals</b>\n\n".to_string();
        for p in &pairs {
            let symbol = p["baseToken"]["symbol"].as_str().unwrap_or("???");
            let change1h = p["priceChange"]["h1"].as_f64().unwrap_or(0.0);
            let mc = p["fdv"].as_f64().map(|v| format!("${v:.0}")).unwrap_or_else(|| "N/A".to_string());
            msg += &format!("• <b>{symbol}</b> {change1h:.1}% (1h) | MC: {mc}\n");
        }
        self.tg.send_html(chat_id, &msg, Some(kb::main_only())).await?;
        Ok(())
    }
}

fn check_pin(user: &UserRecord, attempt: &str) -> bool {
    match &user.pin_hash {
        Some(h) => *h == hash_pin(attempt),
        None => true,
    }
}

fn short_wallet(w: &str) -> String {
    if w.len() < 8 {
        return w.to_string();
    }
    format!("{}...{}", &w[..4], &w[w.len() - 4..])
}
