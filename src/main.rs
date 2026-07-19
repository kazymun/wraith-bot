mod config;
mod crypto;
mod db;
mod dexscreener;
mod handlers;
mod jupiter;
mod keyboards;
mod pumpportal;
mod rpc;
mod state;
mod telegram;
mod wallet;

use config::Config;
use crypto::Crypto;
use db::Db;
use handlers::App;
use jupiter::Jupiter;
use rpc::SolanaRpc;
use telegram::TgClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;

    let app = App::new(
        TgClient::new(cfg.telegram_token.clone()),
        Db::open(&cfg.db_path)?,
        Crypto::new(&cfg.pepper_b64)?,
        SolanaRpc::new(cfg.rpc_url.clone()),
        Jupiter::new(cfg.jupiter_api_key.clone()),
        cfg.default_slippage_bps,
        cfg.fee_wallet.clone(),
        cfg.min_pin_length,
    );

    // Verify the platform fee wallet's wrapped-SOL token account actually
    // exists on-chain. This is the #1 cause of "fees worked once, then
    // stopped": Jupiter needs a pre-existing token account to deliver the
    // platform fee into, and does not create one for you as a side effect
    // of the swap. If this account was never created -- or was created
    // then later closed by unwrapping/withdrawing everything out of it in
    // a wallet app -- every single trade will silently collect $0 in fees
    // even though `platformFeeBps` is set on every quote.
    if !cfg.fee_wallet.is_empty() {
        match jupiter::derive_wsol_fee_account(&cfg.fee_wallet) {
            Ok(fee_ata) => match app.rpc.get_account_exists(&fee_ata).await {
                Ok(true) => println!("✅ Fee wallet WSOL account exists ({fee_ata}) — platform fees can be collected."),
                Ok(false) => eprintln!(
                    "🚨 Fee wallet WSOL account ({fee_ata}) does NOT exist on-chain yet! \
                     Every buy/sell will silently collect $0 in platform fees until this \
                     account is created. Create it once with `spl-token create-account \
                     So11111111111111111111111111111111111111112 --owner {}` (or any \
                     'wrap SOL' action pointed at that owner), then restart the bot. \
                     Also make sure nothing ever fully unwraps/closes this account again.",
                    cfg.fee_wallet
                ),
                Err(e) => eprintln!("⚠️ Couldn't verify fee wallet WSOL account ({fee_ata}): {e:?}"),
            },
            Err(e) => eprintln!("⚠️ FEE_WALLET is set but invalid: {e}"),
        }
    }

    println!("👻 Wraith bot is running...");

    // Spawn background gem scanner (DexScreener-based)
    let scanner_app = app.clone();
    tokio::spawn(async move {
        scanner_app.run_gem_scanner().await;
    });

    // Spawn the PumpPortal WebSocket listener -- the earliest possible
    // signal, since tokens land here at creation, before DexScreener has
    // any listing for them. Runs alongside the DexScreener scanner, not
    // instead of it.
    let (pump_tx, mut pump_rx) = tokio::sync::mpsc::channel::<pumpportal::PumpEvent>(1000);
    tokio::spawn(async move {
        pumpportal::run(pump_tx).await;
    });
    let pump_app = app.clone();
    tokio::spawn(async move {
        while let Some(event) = pump_rx.recv().await {
            pump_app.handle_pump_event(event).await;
        }
    });

    let mut offset: i64 = 0;
    loop {
        let updates = match app.tg.get_updates(offset).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("Failed to get updates: {e}. Retrying in 3s...");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            if let Some(msg) = update.message {
                let chat_id = msg.chat.id;
                if let Err(e) = app.handle_message(msg).await {
                    eprintln!("Error handling message: {e}");
                    let _ = app.tg.send_html(
                        chat_id,
                        "⚠️ Something went wrong loading your account. This has been logged — please try again in a moment.",
                        None,
                    ).await;
                }
            } else if let Some(cb) = update.callback_query {
                let chat_id = cb.message.as_ref().map(|m| m.chat.id);
                let message_id = cb.message.as_ref().map(|m| m.message_id);
                if let Some(chat_id) = chat_id {
                    if let Some(data) = &cb.data {
                        if let Err(e) = app.handle_callback(&cb.id, data, chat_id, cb.from.id, message_id).await {
                            eprintln!("Error handling callback: {e}");
                        }
                    }
                }
            }
        }
    }
}
